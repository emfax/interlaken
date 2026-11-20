//! Streaming a statement's rows to the caller (ADR-0031).

use std::time::Instant;

use arrow_array::RecordBatch;
use futures::StreamExt;
use interlaken_exec::{BatchStream, ExecError, Statement};
use interlaken_ir::Type;

use super::connection::Session;
use super::result::QueryResult;
use crate::error::EngineError;

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
/// - **Dropped before its end:** execution stops, its writes are undone,
///   and an implicit transaction is rolled back. An open transaction is
///   not aborted: the statement is as if it had never run.
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
    /// When the statement times out, if it has a timeout.
    deadline: Option<Instant>,
}

impl<'s> QueryStream<'s> {
    /// Construct the stream of a begun statement executing as `batches`.
    pub(super) fn new(
        session: &'s mut Session,
        statement: Statement,
        implicit: bool,
        columns: Vec<String>,
        types: Vec<Type>,
        batches: BatchStream,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            session,
            statement: Some(statement),
            implicit,
            columns,
            types,
            batches: Some(batches),
            deadline,
        }
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
            Some(Ok(batch)) => Some(Ok(batch)),
            Some(Err(e)) => {
                self.batches = None;
                if let Some(statement) = self.statement.take() {
                    self.session.fail_statement(statement, self.implicit);
                }
                Some(Err(e.into()))
            }
            None => {
                self.batches = None;
                let statement = self.statement.take()?;
                match self
                    .session
                    .finish_statement(statement, self.implicit)
                    .await
                {
                    Ok(()) => None,
                    Err(e) => Some(Err(e)),
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
        })
    }
}

impl Drop for QueryStream<'_> {
    /// Cancel the statement if it hasn't ended: stop executing it and undo
    /// its writes, rolling back an implicit transaction.
    fn drop(&mut self) {
        // Dropping the batches first stops execution before the undo.
        self.batches = None;
        if let Some(statement) = self.statement.take() {
            self.session.cancel_statement(statement, self.implicit);
        }
    }
}
