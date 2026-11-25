//! Streaming a statement's rows to the caller (ADR-0031).

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use futures::{Stream, StreamExt};
use interlaken_exec::{BatchStream, ExecError, QueryMemory, Statement, UsageCounters};
use interlaken_ir::Type;

use super::connection::Session;
use super::result::QueryResult;
use super::usage::Usage;
use crate::error::EngineError;
use crate::telemetry;

/// A planned statement's executing batches and what they hold.
pub(super) struct Execution {
    /// The output column names.
    pub(super) columns: Vec<String>,
    /// The output column types.
    pub(super) types: Vec<Type>,
    /// The executing plan's batches.
    pub(super) batches: BatchStream,
    /// Whether the plan writes (DML).
    pub(super) writes: bool,
    /// The statement's query memory, for its peak.
    pub(super) memory: Arc<QueryMemory>,
    /// The statement's kind, for metrics.
    pub(super) kind: &'static str,
}

/// What a statement's usage and metrics are computed from (ADR-0039).
pub(super) struct Meter {
    /// The statement's span.
    pub(super) span: tracing::Span,
    /// When the statement started.
    pub(super) started: Instant,
    /// Time spent parsing.
    pub(super) parse: Duration,
    /// Time spent planning.
    pub(super) plan: Duration,
    /// When execution started.
    pub(super) executing: Instant,
    /// Rows and bytes read from storage.
    pub(super) reads: Arc<UsageCounters>,
    /// Nanoseconds spent polling the plan.
    pub(super) cpu: Arc<AtomicU64>,
    /// The transaction's write set size when the statement started.
    pub(super) written_before: usize,
}

impl Meter {
    /// Record the phase durations of a statement of `kind`.
    fn record_phases(&self, kind: &'static str, execute: Option<Duration>) {
        let phase = |phase: &'static str, time: Duration| {
            metrics::histogram!(telemetry::STATEMENT_DURATION, "kind" => kind, "phase" => phase)
                .record(time.as_secs_f64());
        };
        if !self.parse.is_zero() {
            phase("parse", self.parse);
        }
        phase("plan", self.plan);
        if let Some(execute) = execute {
            phase("execute", execute);
        }
    }

    /// Record a statement that failed before it began executing.
    pub(super) fn record_unplanned(&self, error: &EngineError) {
        let kind = "unplanned";
        self.record_phases(kind, None);
        metrics::counter!(telemetry::STATEMENTS, "kind" => kind, "outcome" => "error").increment(1);
        telemetry::record_failure(error);
        self.span.record("kind", kind);
        self.span.record("outcome", "error");
    }
}

/// A stream that times each poll of the plan, within the execute span.
struct Timed {
    /// The plan's batches.
    inner: BatchStream,
    /// Where to add the time.
    cpu: Arc<AtomicU64>,
    /// The `interlaken.execute` span.
    span: tracing::Span,
}

impl Stream for Timed {
    type Item = Result<RecordBatch, ExecError>;

    /// Poll the plan within the span, adding the time the poll took.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let _entered = this.span.enter();
        let start = Instant::now();
        let poll = this.inner.poll_next_unpin(cx);
        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        this.cpu.fetch_add(nanos, Ordering::Relaxed);
        poll
    }
}

/// Wrap a plan's batches so each poll's time adds to `cpu`, within an
/// `interlaken.execute` span under `parent`.
pub(super) fn timed(
    inner: BatchStream,
    cpu: Arc<AtomicU64>,
    parent: &tracing::Span,
) -> BatchStream {
    let span = tracing::info_span!(parent: parent, "interlaken.execute");
    Timed { inner, cpu, span }.boxed()
}

/// How a statement ended, for its metrics.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// It ran to its end and its writes were kept.
    Ok,
    /// It failed, and its writes were undone.
    Error,
    /// Its stream was dropped before its end.
    Cancelled,
}

/// The rows of a running query or DML statement, read a batch at a time
/// (from [`Session::query_stream`]).
///
/// The statement executes as batches are read, and holds only what its
/// operators need, so a result of any size streams in bounded memory. The
/// stream borrows its session mutably: the session runs nothing else until
/// the stream is dropped.
///
/// The statement ends when the stream does:
///
/// - **Read to the end** ([`QueryStream::next`] returns `None`, or
///   [`QueryStream::finish`] or [`QueryStream::collect`]): its writes are
///   kept, and an implicit transaction commits.
/// - **An error:** its writes are undone, and the open transaction is
///   aborted (an implicit one is rolled back). The error is returned once;
///   the stream then ends.
/// - **Dropped before its end:** a query stops, and is as if it had never
///   run: an implicit transaction is rolled back, and an open one is not
///   aborted. A DML statement runs to its end anyway, as in Postgres, and
///   its remaining rows are discarded: it ends as if read to the end (an
///   implicit transaction commits) or, if it fails, as for an error. It
///   finishes in the background, and the session's next call waits for
///   it, so the next statement sees its writes. A failure is reported to
///   no one, but aborts an open transaction, so its commit then fails;
///   [`Session::rollback`] stops it instead.
///
/// The borrow checker keeps the session from running another statement
/// meanwhile:
///
/// ```compile_fail,E0499
/// # async fn run(session: &mut interlaken_engine::Session) -> Result<(), interlaken_engine::EngineError> {
/// let mut rows = session.query_stream("(plan (get docs))", Vec::new()).await?;
/// session.query("(plan (get docs))", Vec::new()).await?; // `session` is borrowed by `rows`
/// rows.next().await;
/// # Ok(())
/// # }
/// ```
#[must_use = "a query stream runs its statement only as it is read"]
pub struct QueryStream<'s> {
    /// The session running the statement.
    session: &'s mut Session,
    /// The statement, until it ends.
    statement: Option<Statement>,
    /// Whether the statement runs in its own (implicit) transaction.
    implicit: bool,
    /// The output column names.
    columns: Vec<String>,
    /// The output column types.
    types: Vec<Type>,
    /// The executing plan's batches, until the statement ends.
    batches: Option<BatchStream>,
    /// Whether the statement writes (DML), so it runs to its end even if
    /// the stream is dropped.
    writes: bool,
    /// When the statement times out, if it has a timeout.
    deadline: Option<Instant>,
    /// What its usage is computed from.
    meter: Meter,
    /// The statement's query memory.
    memory: Arc<QueryMemory>,
    /// The statement's kind, for metrics.
    kind: &'static str,
    /// Rows returned so far.
    rows_returned: u64,
    /// The statement's usage, once it has ended.
    usage: Option<Usage>,
    /// The implicit transaction's commit timestamp, once it has committed.
    commit_ts: Option<crate::CommitTs>,
}

impl<'s> QueryStream<'s> {
    /// Construct the stream of a begun statement executing as `run`.
    pub(super) fn new(
        session: &'s mut Session,
        statement: Statement,
        implicit: bool,
        run: Execution,
        deadline: Option<Instant>,
        meter: Meter,
    ) -> Self {
        let batches = timed(run.batches, Arc::clone(&meter.cpu), &meter.span);
        meter.span.record("kind", run.kind);
        Self {
            session,
            statement: Some(statement),
            implicit,
            columns: run.columns,
            types: run.types,
            batches: Some(batches),
            writes: run.writes,
            deadline,
            meter,
            memory: run.memory,
            kind: run.kind,
            rows_returned: 0,
            usage: None,
            commit_ts: None,
        }
    }

    /// Return the statement's usage once it has ended (ADR-0039), or `None`
    /// while it runs.
    #[must_use]
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Return the commit timestamp of a statement that ran in its own
    /// (implicit) transaction and wrote something, once it has ended
    /// (ADR-0040); `None` otherwise.
    #[must_use]
    pub fn commit_ts(&self) -> Option<crate::CommitTs> {
        self.commit_ts
    }

    /// End the statement's accounting: compute its usage, record its
    /// metrics and span fields, and add it to the session's transaction.
    fn account(&mut self, outcome: Outcome, bytes_written: usize, error: Option<&EngineError>) {
        let meter = &self.meter;
        let usage = Usage {
            rows_scanned: meter.reads.rows_scanned(),
            rows_returned: self.rows_returned,
            bytes_read: meter.reads.bytes_read(),
            bytes_written: u64::try_from(bytes_written).unwrap_or(u64::MAX),
            cpu: Duration::from_nanos(meter.cpu.load(Ordering::Relaxed)),
            peak_memory: u64::try_from(self.memory.peak()).unwrap_or(u64::MAX),
            elapsed: meter.started.elapsed(),
        };
        let kind = self.kind;
        let outcome = match outcome {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::Cancelled => "cancelled",
        };
        meter.record_phases(kind, Some(meter.executing.elapsed()));
        metrics::counter!(telemetry::STATEMENTS, "kind" => kind, "outcome" => outcome).increment(1);
        metrics::histogram!(telemetry::STATEMENT_CPU, "kind" => kind)
            .record(usage.cpu.as_secs_f64());
        metrics::counter!(telemetry::ROWS_SCANNED).increment(usage.rows_scanned);
        metrics::counter!(telemetry::ROWS_RETURNED).increment(usage.rows_returned);
        metrics::counter!(telemetry::BYTES_READ).increment(usage.bytes_read);
        metrics::counter!(telemetry::BYTES_WRITTEN).increment(usage.bytes_written);
        #[expect(clippy::cast_precision_loss, reason = "metrics are approximate")]
        metrics::histogram!(telemetry::QUERY_MEMORY_PEAK).record(usage.peak_memory as f64);
        if let Some(error) = error {
            telemetry::record_failure(error);
        }
        let span = &meter.span;
        span.record("outcome", outcome);
        span.record("rows_scanned", usage.rows_scanned);
        span.record("rows_returned", usage.rows_returned);
        span.record("bytes_read", usage.bytes_read);
        span.record("bytes_written", usage.bytes_written);
        span.record("cpu_seconds", usage.cpu.as_secs_f64());
        span.record("peak_memory", usage.peak_memory);
        self.session.add_usage(usage);
        self.usage = Some(usage);
    }

    /// Return the output column names; unnamed columns are `?column?`, as in Postgres.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Return the output columns' IR types.
    #[must_use]
    pub fn types(&self) -> &[Type] {
        &self.types
    }

    /// Return the next batch of rows, executing the statement until it
    /// produces one; `None` once the statement has ended.
    ///
    /// At the end of the rows the statement ends successfully: its writes
    /// are kept, and an implicit transaction commits, whose failure is
    /// returned here instead of `None`.
    ///
    /// # Errors
    ///
    /// Returns the execution error (SQLSTATE `57014` past the statement
    /// timeout), after undoing the statement and aborting the open
    /// transaction or rolling back the implicit one; or the commit's
    /// error. Either way the stream then ends.
    pub async fn next(&mut self) -> Option<Result<RecordBatch, EngineError>> {
        let batches = self.batches.as_mut()?;
        let pulled = match self.deadline {
            Some(deadline) => tokio::time::timeout_at(deadline.into(), batches.next())
                .await
                .unwrap_or(Some(Err(ExecError::QueryCanceled))),
            None => batches.next().await,
        };
        match pulled {
            Some(Ok(batch)) => {
                self.rows_returned += batch.num_rows() as u64;
                Some(Ok(batch))
            }
            Some(Err(e)) => {
                self.batches = None;
                let error = EngineError::from(e);
                if let Some(statement) = self.statement.take() {
                    self.session.fail_statement(statement, self.implicit);
                    self.account(Outcome::Error, 0, Some(&error));
                }
                Some(Err(error))
            }
            None => {
                self.batches = None;
                let statement = self.statement.take()?;
                let written = self
                    .session
                    .write_set_bytes()
                    .saturating_sub(self.meter.written_before);
                match self
                    .session
                    .finish_statement(statement, self.implicit)
                    .await
                {
                    Ok(commit_ts) => {
                        self.commit_ts = commit_ts;
                        self.account(Outcome::Ok, written, None);
                        None
                    }
                    Err(e) => {
                        self.account(Outcome::Error, 0, Some(&e));
                        Some(Err(e))
                    }
                }
            }
        }
    }

    /// Run the statement to its end, discarding its remaining rows, and end
    /// it successfully.
    ///
    /// # Errors
    ///
    /// As for [`QueryStream::next`].
    pub async fn finish(mut self) -> Result<(), EngineError> {
        while let Some(batch) = self.next().await {
            batch?;
        }
        Ok(())
    }

    /// Collect the remaining rows and end the statement successfully.
    ///
    /// # Errors
    ///
    /// As for [`QueryStream::next`].
    pub async fn collect(mut self) -> Result<QueryResult, EngineError> {
        let mut batches = Vec::new();
        while let Some(batch) = self.next().await {
            batches.push(batch?);
        }
        Ok(QueryResult {
            columns: std::mem::take(&mut self.columns),
            types: std::mem::take(&mut self.types),
            batches,
            usage: self.usage.unwrap_or_default(),
            commit_ts: self.commit_ts,
        })
    }
}

impl Drop for QueryStream<'_> {
    /// End the statement if it hasn't ended: run DML to its end in the
    /// background; stop a query and undo it, rolling back an implicit
    /// transaction.
    fn drop(&mut self) {
        let Some(statement) = self.statement.take() else {
            return;
        };
        // A DML statement finishing in the background is counted up to here.
        self.account(Outcome::Cancelled, 0, None);
        match (self.writes, self.batches.take()) {
            (true, Some(batches)) => {
                self.session
                    .finish_in_background(statement, self.implicit, batches, self.deadline)
            }
            (_, batches) => {
                // Dropping the batches first stops execution before the undo.
                drop(batches);
                self.session.cancel_statement(statement, self.implicit);
            }
        }
    }
}
