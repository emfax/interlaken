//! Sessions: a principal's statements, in transactions (ADR-0031).

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use interlaken_exec::{
    BatchStream, ExecError, ExplainOptions, Op, Physical, Statement, Transaction, UsageCounters,
    execute, explain, explain_analyze,
};
use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Datum, ExecOptions, check_params};
use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_ir::{DataType, Plan, QualifiedName, SchemaVersion};
use interlaken_storage::{Freshness, SavepointId, Storage, Txn, TxnOptions};
use tokio::task::JoinHandle;
use tracing::Instrument;

use super::engine::Engine;
use super::pipeline::{Step, exec_context, parse, pin, prepare};
use super::result::QueryResult;
use super::stream::{Execution, Meter, QueryStream};
use super::usage::Usage;
use crate::catalog::{
    self, ColumnSpec, EntitySpec, ForeignKeySpec, IndexSpec, MembershipSpec, ObjectKind, TableSpec,
};
use crate::error::EngineError;
use crate::policy::{
    GateResult, PolicyDocument, PolicySettings, activate, set_policy_settings, submit_proposal,
    verify_locally,
};
use crate::privileges::{self, Privilege, Privileges};
use crate::telemetry;

/// Count one step of a policy proposal (ADR-0030) and its outcome.
fn record_proposal<T>(step: &'static str, result: &Result<T, EngineError>) {
    let outcome = if result.is_ok() { "ok" } else { "error" };
    metrics::counter!(telemetry::POLICY_PROPOSALS, "step" => step, "outcome" => outcome)
        .increment(1);
}

/// Return a new statement span, whose fields are recorded as it runs.
fn statement_span() -> tracing::Span {
    tracing::info_span!(
        "interlaken.statement",
        kind = tracing::field::Empty,
        outcome = tracing::field::Empty,
        rows_scanned = tracing::field::Empty,
        rows_returned = tracing::field::Empty,
        bytes_read = tracing::field::Empty,
        bytes_written = tracing::field::Empty,
        cpu_seconds = tracing::field::Empty,
        peak_memory = tracing::field::Empty,
    )
}

/// Return a query or DML plan's kind, for metrics: the first operator
/// that writes, or `select` if none does.
fn statement_kind(node: &Physical) -> &'static str {
    match &node.op {
        Op::Insert { .. } => "insert",
        Op::Update { .. } => "update",
        Op::Delete { .. } => "delete",
        op => op
            .inputs()
            .into_iter()
            .map(statement_kind)
            .find(|&kind| kind != "select")
            .unwrap_or("select"),
    }
}

/// What a statement may change, which decides the privilege it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A query or DML statement: governed by policies, no privilege needed.
    Query,
    /// DDL: needs `admin`; the transaction's catalog is then uncommitted.
    Catalog,
    /// A policy change: needs `admin`; the transaction's policies are then uncommitted.
    Policy,
    /// Another administrative change (privileges, verification records).
    Admin,
}

/// Check that the principal may change the namespaces in `namespaces`:
/// administrators may change anything, and an owner its own namespaces
/// (ADR-0042). With no namespaces, only administrators may.
async fn may_change(step: &Step<'_>, namespaces: &[&str]) -> Result<(), EngineError> {
    if step.privileges.admin {
        return Ok(());
    }
    if namespaces.is_empty() {
        return Err(EngineError::PermissionDenied(
            "this change requires the admin privilege".into(),
        ));
    }
    for namespace in namespaces {
        if !catalog::owns(step.txn, &step.context.principal, namespace).await? {
            return Err(EngineError::PermissionDenied(format!(
                "changing namespace {namespace} requires owning it or the admin privilege"
            )));
        }
    }
    Ok(())
}

/// Check that the principal may verify or activate proposal `proposal`:
/// administrators any, and a namespace's owner its namespace's (ADR-0042).
async fn may_change_policies(step: &Step<'_>, proposal: i64) -> Result<(), EngineError> {
    match crate::policy::proposal_scope(step.txn, proposal).await? {
        Some(scope) => may_change(step, &[scope.as_str()]).await,
        None => may_change(step, &[]).await,
    }
}

/// Return the stored name of an existing object of `kind` named `name`,
/// looked up through the session's search path.
async fn existing(step: &Step<'_>, kind: ObjectKind, name: &str) -> Result<String, EngineError> {
    catalog::resolve_existing(step.txn, step.search_path, kind, name).await
}

/// An open transaction.
struct Open {
    /// The statement runner (ADR-0026).
    exec: Transaction,
    /// Evaluation settings fixed at the transaction's start.
    options: ExecOptions,
    /// DDL ran in this transaction.
    catalog_dirty: bool,
    /// A policy change ran in this transaction.
    policies_dirty: bool,
}

/// A DML statement whose stream was dropped before its end, running to
/// its end in a background task that owns the transaction meanwhile.
struct Finishing {
    /// Whether the statement runs in its own (implicit) transaction, which
    /// the task commits; otherwise the task returns the open transaction.
    implicit: bool,
    /// The task: the open transaction once the statement has ended, or
    /// `None` for an implicit one.
    task: JoinHandle<Option<Open>>,
}

/// A connection's worth of statements, run for one principal.
///
/// Statements run in the open transaction, or each in its own transaction
/// when none is open (autocommit). Each statement is atomic: if it fails,
/// its writes are undone, and an explicit transaction accepts nothing more
/// until it is rolled back (to a savepoint or entirely), as in Postgres.
///
/// Queries are IR text (or built [`Plan`]s). Policies govern every query.
/// DDL, policy changes and privileges are methods that need the `admin`
/// privilege.
pub struct Session {
    /// The engine.
    engine: Engine,
    /// The principal and request facts.
    context: SecurityContext,
    /// How long a statement may run before it is cancelled.
    statement_timeout: Option<Duration>,
    /// The open transaction, if any, unless a dropped statement is
    /// finishing in it.
    txn: Option<Open>,
    /// A dropped DML statement still running to its end, if any.
    finishing: Option<Finishing>,
    /// The usage of the current or last transaction's statements.
    usage: Usage,
    /// The key the session's usage is attributed to, if attribution is on.
    attribution: Option<Arc<str>>,
    /// The namespaces unqualified names are looked up in (ADR-0042).
    search_path: Vec<String>,
}

impl Session {
    /// Construct a session with no transaction open.
    pub(crate) fn new(engine: Engine, context: SecurityContext) -> Self {
        let attribution = engine.inner.attributor.key(&context);
        Self {
            attribution,
            engine,
            context,
            statement_timeout: None,
            txn: None,
            finishing: None,
            usage: Usage::default(),
            search_path: vec![crate::catalog::PUBLIC.to_owned()],
        }
    }

    /// Set the namespaces unqualified names are looked up in, in order
    /// (ADR-0042). New tables and other objects named without a namespace
    /// go in the first. The default is `["public"]`; namespaces that don't
    /// exist are skipped.
    pub fn set_search_path<I, S>(&mut self, namespaces: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.search_path = namespaces.into_iter().map(Into::into).collect();
    }

    /// Return the namespaces unqualified names are looked up in.
    #[must_use]
    pub fn search_path(&self) -> &[String] {
        &self.search_path
    }

    /// Return the principal and request facts statements run with.
    #[must_use]
    pub fn context(&self) -> &SecurityContext {
        &self.context
    }

    /// Cancel any statement that runs longer than `timeout` (`None` for no limit).
    ///
    /// A cancelled statement fails with SQLSTATE `57014` and its writes are undone.
    pub fn set_statement_timeout(&mut self, timeout: Option<Duration>) {
        self.statement_timeout = timeout;
    }

    /// Return the summed usage of the query and DML statements of the open
    /// transaction, or of the last one if none is open (ADR-0039). A
    /// statement run outside a transaction is its own transaction.
    #[must_use]
    pub fn transaction_usage(&self) -> Usage {
        self.usage
    }

    /// Add a statement's usage to its transaction's, and to its
    /// attribution key's metrics if attribution is on.
    pub(super) fn add_usage(&mut self, usage: Usage) {
        self.usage += usage;
        if let Some(key) = &self.attribution {
            let key = key.to_string();
            metrics::counter!(telemetry::USAGE_STATEMENTS, "attribution" => key.clone())
                .increment(1);
            metrics::counter!(telemetry::USAGE_ROWS_SCANNED, "attribution" => key.clone())
                .increment(usage.rows_scanned);
            metrics::counter!(telemetry::USAGE_BYTES_READ, "attribution" => key.clone())
                .increment(usage.bytes_read);
            metrics::counter!(telemetry::USAGE_BYTES_WRITTEN, "attribution" => key.clone())
                .increment(usage.bytes_written);
            metrics::gauge!(telemetry::USAGE_CPU, "attribution" => key)
                .increment(usage.cpu.as_secs_f64());
        }
    }

    /// Return the open transaction's write set size, or zero if none is open.
    pub(super) fn write_set_bytes(&self) -> usize {
        self.txn
            .as_ref()
            .map_or(0, |open| open.exec.txn().write_set_bytes())
    }

    /// Return whether a transaction is open.
    #[must_use]
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some() || self.finishing.as_ref().is_some_and(|f| !f.implicit)
    }

    /// Return whether the open transaction is aborted: a statement failed
    /// and only a rollback is accepted.
    ///
    /// This first waits for a dropped DML statement to finish, since its
    /// failure would abort the transaction.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Invalid`] if a dropped statement's task
    /// panicked, losing the open transaction.
    pub async fn is_aborted(&mut self) -> Result<bool, EngineError> {
        self.settle().await?;
        Ok(self.txn.as_ref().is_some_and(|open| open.exec.is_aborted()))
    }

    /// Wait for a dropped DML statement to finish (see [`QueryStream`]),
    /// taking back the open transaction it ran in.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Invalid`] if the statement's task panicked
    /// in an explicit transaction, which is then lost (rolled back).
    async fn settle(&mut self) -> Result<(), EngineError> {
        let Some(finishing) = self.finishing.take() else {
            return Ok(());
        };
        match finishing.task.await {
            Ok(open) => {
                self.txn = open;
                Ok(())
            }
            // An implicit transaction is the dropped statement's own; its
            // failure leaves nothing behind for later statements.
            Err(_) if finishing.implicit => Ok(()),
            Err(e) => Err(EngineError::Invalid(format!(
                "the open transaction was lost finishing a dropped statement: {e}"
            ))),
        }
    }

    /// Run a streamed DML statement its caller stopped reading to its end
    /// in the background, discarding its rows, then end it as
    /// [`QueryStream::next`] would: keep its writes, or undo them and abort
    /// the transaction if it fails; an implicit transaction then commits
    /// or rolls back.
    ///
    /// Without a Tokio runtime to run it on, the statement is undone
    /// instead, as a cancelled read is.
    pub(super) fn finish_in_background(
        &mut self,
        statement: Statement,
        implicit: bool,
        batches: BatchStream,
        deadline: Option<Instant>,
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            drop(batches);
            self.cancel_statement(statement, implicit);
            return;
        };
        let Some(mut open) = self.txn.take() else {
            return;
        };
        let task = runtime.spawn(async move {
            let kept = match drain(batches, deadline).await {
                Ok(()) => open.exec.release_statement(statement).is_ok(),
                Err(_) => {
                    // The transaction is aborted even if the undo fails.
                    let _ = open.exec.rollback_statement(statement);
                    false
                }
            };
            if !implicit {
                return Some(open);
            }
            if kept {
                // Nobody is left to hear a commit failure; the writes are
                // then simply not committed, as for any failed statement.
                let _ = open.exec.commit().await;
            } else {
                open.exec.rollback();
            }
            None
        });
        self.finishing = Some(Finishing { implicit, task });
    }

    /// Open a transaction.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::TransactionInProgress`] if one is open, or a storage error.
    pub async fn begin(&mut self, options: TxnOptions) -> Result<(), EngineError> {
        self.settle().await?;
        if self.txn.is_some() {
            return Err(EngineError::TransactionInProgress);
        }
        let begun = self.engine.inner.storage.begin(options);
        let txn = match (options.freshness, self.statement_timeout) {
            // Waiting for a commit is bounded by the statement timeout.
            (Freshness::AtLeast(ts), Some(timeout)) => tokio::time::timeout(timeout, begun)
                .await
                .map_err(|_| EngineError::NotVisible(ts))??,
            _ => begun.await?,
        };
        self.install(txn);
        Ok(())
    }

    /// Open a transaction on `txn`, which storage has begun: for reading at
    /// a past snapshot (ADR-0040).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::TransactionInProgress`] if one is open.
    pub(crate) async fn begin_on(&mut self, txn: Txn) -> Result<(), EngineError> {
        self.settle().await?;
        if self.txn.is_some() {
            return Err(EngineError::TransactionInProgress);
        }
        self.install(txn);
        Ok(())
    }

    /// Make `txn` the open transaction.
    fn install(&mut self, txn: Txn) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        let options = ExecOptions {
            now,
            seed: now.cast_unsigned(),
        };
        // Statements plan against the catalog they load; the runner's own
        // catalog is only a placeholder.
        let placeholder = Arc::new(InMemoryCatalog::new(SchemaVersion::new(0)));
        let exec = Transaction::new(txn, placeholder, options).with_memory(
            self.engine.inner.query_budget,
            self.engine.inner.pool.clone(),
        );
        self.txn = Some(Open {
            exec,
            options,
            catalog_dirty: false,
            policies_dirty: false,
        });
        self.usage = Usage::default();
    }

    /// Commit the open transaction, or roll it back if it is aborted.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::NoTransaction`] if none is open, or the
    /// commit's error (a serialization failure is SQLSTATE `40001`; the
    /// caller decides whether to retry).
    ///
    /// On success, returns the commit's timestamp (ADR-0040), or `None` if
    /// the transaction wrote nothing. Another member of the cluster can
    /// wait for it with [`Engine::wait_visible`](crate::Engine::wait_visible).
    pub async fn commit(&mut self) -> Result<Option<crate::CommitTs>, EngineError> {
        self.settle().await?;
        let open = self.txn.take().ok_or(EngineError::NoTransaction)?;
        Ok(open.exec.commit().await?)
    }

    /// Return the engine the session belongs to.
    pub(crate) fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Follow `table`'s committed changes as this session's principal may
    /// see them, after `from`, or after the last visible commit if `from`
    /// is `None` (ADR-0040).
    ///
    /// Each batch is one committed transaction's changes to rows the
    /// principal can read before or after it, read through this session's
    /// policies with the policies and entities of each commit: a row that
    /// becomes readable arrives as an insert, and one that stops being
    /// readable leaves as a delete. Rows carry the session's column masks.
    /// The subscription runs in a session of its own, with this session's
    /// request context, and doesn't use this session's transaction.
    ///
    /// # Errors
    ///
    /// Returns SQLSTATE `42P01` if the table doesn't exist, or `55000` if
    /// `from` is older than the change log or version GC reaches back.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(session: &interlaken_engine::Session) -> Result<(), interlaken_engine::EngineError> {
    /// let mut subscription = session.subscribe("docs", None).await?;
    /// while let Some(batch) = subscription.next().await {
    ///     for change in batch?.changes {
    ///         println!("{:?} {:?}", change.kind, change.key);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe(
        &self,
        table: &str,
        from: Option<crate::CommitTs>,
    ) -> Result<crate::Subscription, EngineError> {
        let session = self.engine.session(self.context.clone());
        crate::Subscription::open(session, table, from).await
    }

    /// Return the snapshot the open transaction reads at, or `None` if no
    /// transaction is open (ADR-0040). Under READ COMMITTED it is the
    /// current statement's.
    ///
    /// Reading tables in a read-only transaction and then following a change
    /// feed from its snapshot sees every change exactly once.
    #[must_use]
    pub fn snapshot_ts(&self) -> Option<crate::CommitTs> {
        self.txn.as_ref().map(|open| open.exec.snapshot())
    }

    /// Roll the open transaction back.
    ///
    /// A dropped DML statement still finishing in it is stopped, since its
    /// writes are discarded anyway.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::NoTransaction`] if none is open.
    pub fn rollback(&mut self) -> Result<(), EngineError> {
        if let Some(finishing) = self.finishing.take_if(|f| !f.implicit) {
            // Stopping the task drops the transaction it owns, aborting it.
            finishing.task.abort();
            return Ok(());
        }
        let open = self.txn.take().ok_or(EngineError::NoTransaction)?;
        open.exec.rollback();
        Ok(())
    }

    /// Return the open transaction's runner.
    fn open(&mut self) -> Result<&mut Open, EngineError> {
        self.txn.as_mut().ok_or(EngineError::NoTransaction)
    }

    /// Open a savepoint in the open transaction.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::NoTransaction`] if none is open, or
    /// SQLSTATE `25P02` if it is aborted.
    pub async fn savepoint(&mut self) -> Result<SavepointId, EngineError> {
        self.settle().await?;
        Ok(self.open()?.exec.savepoint()?)
    }

    /// Undo everything since `savepoint`, leaving the aborted state.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::NoTransaction`] if none is open, or a storage
    /// error if the savepoint is not open.
    pub async fn rollback_to(&mut self, savepoint: SavepointId) -> Result<(), EngineError> {
        self.settle().await?;
        Ok(self.open()?.exec.rollback_to(savepoint)?)
    }

    /// Release `savepoint`, keeping its writes.
    ///
    /// # Errors
    ///
    /// As for [`Session::savepoint`].
    pub async fn release(&mut self, savepoint: SavepointId) -> Result<(), EngineError> {
        self.settle().await?;
        Ok(self.open()?.exec.release(savepoint)?)
    }

    /// Run `body` as one statement of `kind`, in the open transaction or in
    /// its own.
    async fn run<T>(
        &mut self,
        kind: Kind,
        body: impl AsyncFnOnce(&Step<'_>) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        let label = match kind {
            Kind::Catalog => "ddl",
            Kind::Policy => "policy",
            Kind::Admin => "admin",
            Kind::Query => "other",
        };
        let span = tracing::info_span!(
            "interlaken.statement",
            kind = label,
            outcome = tracing::field::Empty,
        );
        let started = Instant::now();
        let result = self
            .run_unmetered(kind, body)
            .instrument(span.clone())
            .await;
        let outcome = if result.is_ok() { "ok" } else { "error" };
        span.record("outcome", outcome);
        metrics::counter!(telemetry::STATEMENTS, "kind" => label, "outcome" => outcome)
            .increment(1);
        metrics::histogram!(
            telemetry::STATEMENT_DURATION,
            "kind" => label,
            "phase" => "execute"
        )
        .record(started.elapsed().as_secs_f64());
        if let Err(error) = &result {
            telemetry::record_failure(error);
        }
        result
    }

    /// Run `body` as [`Session::run`] does, without recording metrics.
    async fn run_unmetered<T>(
        &mut self,
        kind: Kind,
        body: impl AsyncFnOnce(&Step<'_>) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        self.settle().await?;
        let implicit = self.txn.is_none();
        if implicit {
            self.begin(TxnOptions::default()).await?;
        }
        let result = self.run_in_open(kind, body).await;
        if !implicit {
            return result;
        }
        match result {
            Ok(value) => self.commit().await.map(|_| value),
            Err(e) => {
                self.rollback()?;
                Err(e)
            }
        }
    }

    /// Run `body` as one statement of `kind` in the open transaction.
    async fn run_in_open<T>(
        &mut self,
        kind: Kind,
        body: impl AsyncFnOnce(&Step<'_>) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        let engine = self.engine.inner.clone();
        let context = &self.context;
        let search_path = &self.search_path;
        let timeout = self.statement_timeout;
        let open = self.txn.as_mut().ok_or(EngineError::NoTransaction)?;
        let (catalog_dirty, policies_dirty, options) =
            (open.catalog_dirty, open.policies_dirty, open.options);
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let result = open
            .exec
            .statement(async |txn: &Txn| {
                let work = async {
                    let catalog = engine.catalogs.catalog(txn, catalog_dirty).await?;
                    let privileges = privileges::privileges(txn, &context.principal).await?;
                    // Catalog statements check ownership themselves (ADR-0042).
                    if kind == Kind::Admin && !privileges.admin {
                        return Err(EngineError::PermissionDenied(format!(
                            "{} requires the admin privilege",
                            match kind {
                                Kind::Catalog => "changing the catalog",
                                Kind::Policy => "changing policies",
                                Kind::Query | Kind::Admin => "this operation",
                            }
                        )));
                    }
                    let step = Step {
                        engine: &engine,
                        txn,
                        catalog,
                        context,
                        privileges,
                        use_caches: !catalog_dirty && !policies_dirty,
                        options,
                        deadline,
                        search_path,
                    };
                    let value = body(&step).await?;
                    if kind == Kind::Catalog {
                        // Reloading checks the changed declarations fit
                        // together; it must see this statement's writes.
                        txn.next_statement();
                        catalog::load_catalog(txn).await.map_err(|e| match e {
                            // The declarations don't fit together: the
                            // definition is at fault, not the engine.
                            EngineError::Catalog(reason) => EngineError::InvalidDefinition(reason),
                            other => other,
                        })?;
                    }
                    Ok(value)
                };
                match timeout {
                    Some(limit) => tokio::time::timeout(limit, work)
                        .await
                        .unwrap_or(Err(EngineError::Exec(ExecError::QueryCanceled))),
                    None => work.await,
                }
            })
            .await;
        if result.is_ok() {
            match kind {
                Kind::Catalog => open.catalog_dirty = true,
                Kind::Policy => open.policies_dirty = true,
                Kind::Query | Kind::Admin => {}
            }
        }
        result
    }

    /// Run a query or DML statement given as IR text, with parameters
    /// `$1`, `$2`, ..., and collect its rows.
    ///
    /// This collects [`Session::query_stream`]'s batches, so every row is
    /// held at once; stream large results instead.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Syntax`] if the text doesn't parse, a
    /// resolution, verification or security error, [`EngineError::Parameter`]
    /// if `params` don't fit the parameters the statement declares, or the
    /// execution error, after undoing the statement.
    pub async fn query(
        &mut self,
        text: &str,
        params: Vec<Datum>,
    ) -> Result<QueryResult, EngineError> {
        self.query_stream(text, params).await?.collect().await
    }

    /// Run a built query or DML plan, resolved or not, with parameters,
    /// and collect its rows.
    ///
    /// # Errors
    ///
    /// As for [`Session::query`].
    pub async fn query_plan(
        &mut self,
        plan: &Plan,
        params: Vec<Datum>,
    ) -> Result<QueryResult, EngineError> {
        self.query_plan_stream(plan, params).await?.collect().await
    }

    /// Start a query or DML statement given as IR text, with parameters
    /// `$1`, `$2`, ..., and return a stream of its rows, which runs the
    /// statement as it is read.
    ///
    /// The statement is planned before this returns; execution is lazy,
    /// one batch per [`QueryStream::next`], so memory stays bounded by the
    /// plan's operators, not the result's size. The stream borrows the
    /// session, so the session runs nothing else until the stream is
    /// finished or dropped. The statement keeps its semantics (ADR-0026):
    ///
    /// - It runs under its own savepoint in the open transaction, or in an
    ///   implicit transaction committed when the stream ends.
    /// - An error while streaming undoes the statement and aborts an open
    ///   transaction (or rolls back the implicit one), as for
    ///   [`Session::query`].
    /// - Dropping the stream before its end cancels the statement and
    ///   undoes its writes, without aborting an open transaction; the
    ///   implicit transaction is rolled back.
    /// - The statement timeout runs from this call: a stream read past it
    ///   fails with SQLSTATE `57014`, including time the caller spends
    ///   between batches.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Syntax`] if the text doesn't parse, a
    /// resolution, verification or security error, or
    /// [`EngineError::Parameter`] if `params` don't fit the parameters the
    /// statement declares, after undoing the statement; execution errors
    /// arrive through the stream.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(session: &mut interlaken_engine::Session) -> Result<(), interlaken_engine::EngineError> {
    /// let mut rows = session
    ///     .query_stream("(plan (project (get docs :as d) d.id))", Vec::new())
    ///     .await?;
    /// let mut count = 0;
    /// while let Some(batch) = rows.next().await {
    ///     count += batch?.num_rows();
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn query_stream(
        &mut self,
        text: &str,
        params: Vec<Datum>,
    ) -> Result<QueryStream<'_>, EngineError> {
        let (started, span) = (Instant::now(), statement_span());
        let parsed =
            tracing::info_span!(parent: &span, "interlaken.parse").in_scope(|| parse(text));
        let parse_time = started.elapsed();
        match parsed {
            Ok(plan) => {
                self.start_stream(Ok(&plan), params, (started, parse_time, span))
                    .await
            }
            Err(e) => {
                self.start_stream(Err(e), params, (started, parse_time, span))
                    .await
            }
        }
    }

    /// Start a built query or DML plan, resolved or not, with parameters,
    /// and return a stream of its rows.
    ///
    /// # Errors
    ///
    /// As for [`Session::query_stream`].
    pub async fn query_plan_stream(
        &mut self,
        plan: &Plan,
        params: Vec<Datum>,
    ) -> Result<QueryStream<'_>, EngineError> {
        let timing = (Instant::now(), Duration::ZERO, statement_span());
        self.start_stream(Ok(plan), params, timing).await
    }

    /// Begin a query statement (and an implicit transaction if none is
    /// open), plan `plan` and return the stream that executes it. `timing`
    /// is when the statement started, how long parsing took, and its span.
    async fn start_stream(
        &mut self,
        plan: Result<&Plan, EngineError>,
        params: Vec<Datum>,
        timing: (Instant, Duration, tracing::Span),
    ) -> Result<QueryStream<'_>, EngineError> {
        let (started, parse_time, span) = timing;
        let planning = Instant::now();
        self.settle().await?;
        let implicit = self.txn.is_none();
        if implicit {
            self.begin(TxnOptions::default()).await?;
        }
        let timeout = self.statement_timeout;
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let open = self.txn.as_mut().ok_or(EngineError::NoTransaction)?;
        let statement = match open.exec.begin_statement() {
            Ok(statement) => statement,
            Err(e) => {
                if implicit {
                    self.rollback()?;
                }
                return Err(e.into());
            }
        };
        let engine = &self.engine.inner;
        let context = &self.context;
        let search_path = &self.search_path;
        let (catalog_dirty, policies_dirty, options) =
            (open.catalog_dirty, open.policies_dirty, open.options);
        let txn = open.exec.txn();
        let written_before = txn.write_set_bytes();
        let reads = Arc::new(UsageCounters::default());
        let work = async {
            let catalog = engine.catalogs.catalog(txn, catalog_dirty).await?;
            let privileges = privileges::privileges(txn, &context.principal).await?;
            let step = Step {
                engine,
                txn,
                catalog,
                context,
                privileges,
                use_caches: !catalog_dirty && !policies_dirty,
                options,
                deadline,
                search_path,
            };
            let prepared = prepare(&step, plan?).await?;
            check_params(&prepared.params, &params)?;
            let writes = prepared.physical.writes();
            let ctx = exec_context(&step, &prepared, params).with_usage(Arc::clone(&reads));
            let batches = execute(&prepared.physical, &ctx);
            Ok::<_, EngineError>(Execution {
                columns: prepared.columns,
                types: prepared.types,
                batches,
                writes,
                memory: Arc::clone(&ctx.memory),
                kind: statement_kind(&prepared.physical),
            })
        }
        .instrument(tracing::info_span!(parent: &span, "interlaken.plan"));
        let begun = match timeout {
            Some(limit) => tokio::time::timeout(limit, work)
                .await
                .unwrap_or(Err(EngineError::Exec(ExecError::QueryCanceled))),
            None => work.await,
        };
        let meter = Meter {
            span,
            started,
            parse: parse_time,
            plan: planning.elapsed(),
            executing: Instant::now(),
            reads,
            cpu: Arc::default(),
            written_before,
        };
        match begun {
            Ok(run) => Ok(QueryStream::new(
                self, statement, implicit, run, deadline, meter,
            )),
            Err(e) => {
                self.fail_statement(statement, implicit);
                meter.record_unplanned(&e);
                Err(e)
            }
        }
    }

    /// End a streamed statement that ran to its end: keep its writes, and
    /// commit the transaction if it was implicit.
    ///
    /// # Errors
    ///
    /// Returns a storage error, or the commit's error (SQLSTATE `40001` for
    /// a serialization failure).
    pub(super) async fn finish_statement(
        &mut self,
        statement: Statement,
        implicit: bool,
    ) -> Result<Option<crate::CommitTs>, EngineError> {
        let released = self.open().and_then(|open| {
            open.exec
                .release_statement(statement)
                .map_err(EngineError::from)
        });
        if let Err(e) = released {
            if implicit && self.txn.is_some() {
                self.rollback()?;
            }
            return Err(e);
        }
        if implicit {
            return self.commit().await;
        }
        Ok(None)
    }

    /// End a streamed statement that failed: undo its writes and abort the
    /// open transaction, or roll back the implicit one.
    pub(super) fn fail_statement(&mut self, statement: Statement, implicit: bool) {
        if let Some(open) = self.txn.as_mut() {
            // The transaction is aborted even if the undo fails, so nothing
            // the statement wrote can commit.
            let _ = open.exec.rollback_statement(statement);
        }
        if implicit && let Some(open) = self.txn.take() {
            open.exec.rollback();
        }
    }

    /// End a streamed query its caller stopped reading: undo its
    /// writes without aborting the open transaction, or roll back the
    /// implicit one.
    pub(super) fn cancel_statement(&mut self, statement: Statement, implicit: bool) {
        if implicit {
            // Rolling the whole transaction back undoes the statement too.
            drop(statement);
            if let Some(open) = self.txn.take() {
                open.exec.rollback();
            }
            return;
        }
        if let Some(open) = self.txn.as_mut() {
            // A failed undo aborts the transaction instead.
            let _ = open.exec.cancel_statement(statement);
        }
    }

    /// Resolve and verify IR text against the current catalog without
    /// running it, returning the plan pinned to the catalog's schema
    /// version, as a prepared statement keeps it.
    ///
    /// The pinned plan runs with [`Session::query_plan`] until DDL changes
    /// the schema version; then it fails verification (SQLSTATE `0A000`).
    ///
    /// # Errors
    ///
    /// Returns a syntax, resolution or verification error.
    pub async fn prepare(&mut self, text: &str) -> Result<Plan, EngineError> {
        let plan = parse(text);
        self.run(Kind::Query, async move |step: &Step<'_>| {
            Ok(pin(&plan?, step.catalog.as_ref(), step.search_path)?.0)
        })
        .await
    }

    /// Return the physical plan of IR text, one operator per line (ADR-0028).
    ///
    /// With `analyze`, the statement runs (its writes are kept, as in
    /// Postgres) and each operator's metrics are appended. Policy logic is
    /// shown only to principals with the `read-policies` privilege.
    ///
    /// # Errors
    ///
    /// As for [`Session::query`].
    pub async fn explain(&mut self, text: &str, analyze: bool) -> Result<String, EngineError> {
        let plan = parse(text);
        self.run(Kind::Query, async move |step: &Step<'_>| {
            let prepared = prepare(step, &plan?).await?;
            let options = ExplainOptions {
                show_policies: step.privileges.read_policies,
            };
            if analyze {
                check_params(&prepared.params, &[])?;
                let ctx = exec_context(step, &prepared, Vec::new());
                Ok(explain_analyze(&prepared.physical, &ctx, options).await?)
            } else {
                Ok(explain(&prepared.physical, options))
            }
        })
        .await
    }

    /// Return the privileges the session's principal holds.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn privileges(&mut self) -> Result<Privileges, EngineError> {
        self.run(Kind::Query, async |step: &Step<'_>| Ok(step.privileges))
            .await
    }

    /// Create a table.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`] without `admin`, or the
    /// definition's error (see [`catalog::create_table`]).
    pub async fn create_table(&mut self, spec: &TableSpec) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let name = catalog::qualify_new(step.txn, step.search_path, &spec.name).await?;
            may_change(step, &[catalog::namespace_of(&name)]).await?;
            let spec = TableSpec {
                name,
                ..spec.clone()
            };
            catalog::create_table(step.txn, &spec).await
        })
        .await
    }

    /// Drop a table.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::drop_table`]).
    pub async fn drop_table(&mut self, name: &str) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let name = existing(step, ObjectKind::Table, name).await?;
            may_change(step, &[catalog::namespace_of(&name)]).await?;
            catalog::drop_table(step.txn, &name).await
        })
        .await
    }

    /// Add a nullable column to a table.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::add_column`]).
    pub async fn add_column(
        &mut self,
        table: &str,
        column: &ColumnSpec,
    ) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let table = existing(step, ObjectKind::Table, table).await?;
            may_change(step, &[catalog::namespace_of(&table)]).await?;
            catalog::add_column(step.txn, &table, column).await
        })
        .await
    }

    /// Create an index.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::create_index`]).
    pub async fn create_index(&mut self, spec: &IndexSpec) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let table = existing(step, ObjectKind::Table, &spec.table).await?;
            let name = catalog::stored_name(catalog::namespace_of(&table), &spec.name);
            may_change(step, &[catalog::namespace_of(&table)]).await?;
            let spec = IndexSpec {
                name,
                table,
                ..spec.clone()
            };
            catalog::create_index(step.txn, &spec).await
        })
        .await
    }

    /// Drop an index.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::drop_index`]).
    pub async fn drop_index(&mut self, name: &str) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let name = existing(step, ObjectKind::Index, name).await?;
            may_change(step, &[catalog::namespace_of(&name)]).await?;
            catalog::drop_index(step.txn, &name).await
        })
        .await
    }

    /// Declare an entity type.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::declare_entity`]).
    pub async fn declare_entity(&mut self, spec: &EntitySpec) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let table = existing(step, ObjectKind::Table, &spec.table).await?;
            let name = catalog::stored_name(catalog::namespace_of(&table), &spec.name);
            may_change(step, &[catalog::namespace_of(&table)]).await?;
            let mut spec = spec.clone();
            for attribute in &mut spec.attributes {
                if let Some(target) = &attribute.references {
                    attribute.references = Some(existing(step, ObjectKind::Entity, target).await?);
                }
            }
            let spec = EntitySpec {
                name,
                table,
                ..spec
            };
            catalog::declare_entity(step.txn, &spec).await
        })
        .await
    }

    /// Declare a principal type that no table backs, such as `Admin` or
    /// `Service`, so policies can name its principals (ADR-0042).
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::declare_principal_type`]).
    pub async fn declare_principal_type(&mut self, name: &str) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let name = catalog::qualify_new(step.txn, step.search_path, name).await?;
            may_change(step, &[catalog::namespace_of(&name)]).await?;
            catalog::declare_principal_type(step.txn, &name).await
        })
        .await
    }

    /// Remove a principal type that no table backs.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::drop_principal_type`]).
    pub async fn drop_principal_type(&mut self, name: &str) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let name = catalog::qualify_new(step.txn, step.search_path, name).await?;
            may_change(step, &[catalog::namespace_of(&name)]).await?;
            catalog::drop_principal_type(step.txn, &name).await
        })
        .await
    }

    /// Remove an entity type's declaration.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::drop_entity`]).
    pub async fn drop_entity(&mut self, name: &str) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let name = existing(step, ObjectKind::Entity, name).await?;
            may_change(step, &[catalog::namespace_of(&name)]).await?;
            catalog::drop_entity(step.txn, &name).await
        })
        .await
    }

    /// Declare a membership between entity types.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::declare_membership`]).
    pub async fn declare_membership(&mut self, spec: &MembershipSpec) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let spec = MembershipSpec {
                member: existing(step, ObjectKind::Entity, &spec.member).await?,
                group: existing(step, ObjectKind::Entity, &spec.group).await?,
                edges: existing(step, ObjectKind::Table, &spec.edges).await?,
                closure: existing(step, ObjectKind::Table, &spec.closure).await?,
                ..spec.clone()
            };
            may_change(
                step,
                &[
                    catalog::namespace_of(&spec.member),
                    catalog::namespace_of(&spec.group),
                    catalog::namespace_of(&spec.edges),
                    catalog::namespace_of(&spec.closure),
                ],
            )
            .await?;
            catalog::declare_membership(step.txn, &spec).await
        })
        .await
    }

    /// Remove a membership's declaration.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::drop_membership`]).
    pub async fn drop_membership(&mut self, member: &str, group: &str) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let member = existing(step, ObjectKind::Entity, member).await?;
            let group = existing(step, ObjectKind::Entity, group).await?;
            may_change(
                step,
                &[
                    catalog::namespace_of(&member),
                    catalog::namespace_of(&group),
                ],
            )
            .await?;
            catalog::drop_membership(step.txn, &member, &group).await
        })
        .await
    }

    /// Create a foreign key, after checking every existing row satisfies it (ADR-0035).
    ///
    /// Later writes are checked against it when each statement ends, and
    /// deletes carry out its `on_delete` action.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::create_foreign_key`]),
    /// and [`EngineError::ForeignKey`] if an existing row violates it.
    pub async fn create_foreign_key(&mut self, spec: &ForeignKeySpec) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let table = existing(step, ObjectKind::Table, &spec.table).await?;
            let references = existing(step, ObjectKind::Table, &spec.references).await?;
            let name = catalog::stored_name(catalog::namespace_of(&table), &spec.name);
            may_change(
                step,
                &[
                    catalog::namespace_of(&table),
                    catalog::namespace_of(&references),
                ],
            )
            .await?;
            let spec = ForeignKeySpec {
                name,
                table,
                references,
                ..spec.clone()
            };
            catalog::create_foreign_key(step.txn, &spec).await
        })
        .await
    }

    /// Drop a foreign key.
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::drop_foreign_key`]).
    pub async fn drop_foreign_key(&mut self, name: &str) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            let name = existing(step, ObjectKind::ForeignKey, name).await?;
            may_change(step, &[catalog::namespace_of(&name)]).await?;
            catalog::drop_foreign_key(step.txn, &name).await
        })
        .await
    }

    /// Mark a built-in function overload leakproof or not (ADR-0011).
    ///
    /// # Errors
    ///
    /// As for [`Session::create_table`] (see [`catalog::set_function_leakproof`]).
    pub async fn set_function_leakproof(
        &mut self,
        name: &str,
        args: &[DataType],
        leakproof: bool,
    ) -> Result<(), EngineError> {
        let name = QualifiedName::from(name);
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            may_change(step, &[]).await?;
            catalog::set_function_leakproof(step.txn, &name, args, leakproof).await
        })
        .await
    }

    /// Check this session's principal holds `admin`, in an empty
    /// administrative step.
    /// Create namespace `name`, owned by `owner` (ADR-0042). Its owner can
    /// change its schema and policies without `admin`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`] without `admin`, or
    /// [`EngineError::InvalidDefinition`] if the name is taken or invalid.
    pub async fn create_namespace(
        &mut self,
        name: &str,
        owner: &EntityUid,
    ) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            may_change(step, &[]).await?;
            catalog::create_namespace(step.txn, name, owner).await
        })
        .await
    }

    /// Drop namespace `name`: only if it's empty, unless `cascade`, which
    /// drops everything in it, and foreign keys referring to its tables.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`] without `admin`, or
    /// [`EngineError::InvalidDefinition`] (see [`catalog::drop_namespace`]).
    pub async fn drop_namespace(&mut self, name: &str, cascade: bool) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            may_change(step, &[]).await?;
            catalog::drop_namespace(step.txn, name, cascade).await
        })
        .await
    }

    /// Give namespace `name` to `owner`; its owner or an administrator may.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`] for anyone else, or
    /// [`EngineError::InvalidDefinition`] if there is no such namespace.
    pub async fn set_namespace_owner(
        &mut self,
        name: &str,
        owner: &EntityUid,
    ) -> Result<(), EngineError> {
        self.run(Kind::Catalog, async |step: &Step<'_>| {
            may_change(step, &[name]).await?;
            catalog::set_namespace_owner(step.txn, name, owner).await
        })
        .await
    }

    #[cfg(feature = "cluster")]
    async fn require_admin(&mut self) -> Result<(), EngineError> {
        self.run(Kind::Admin, async |_step: &Step<'_>| Ok(())).await
    }

    /// Return the engine's cluster, or an error for a single-node database.
    #[cfg(feature = "cluster")]
    fn cluster(&self) -> Result<std::sync::Arc<interlaken_net::Cluster>, EngineError> {
        self.engine
            .inner
            .cluster
            .clone()
            .ok_or_else(|| EngineError::Catalog("this database isn't a cluster member".into()))
    }

    /// Add node `id`, reachable at `addr`, to the cluster: as a learner
    /// until it catches up, then as a voter (ADR-0037).
    ///
    /// # Errors
    ///
    /// [`EngineError::PermissionDenied`] without `admin`, an error for a
    /// single-node database, or the membership change's error.
    #[cfg(feature = "cluster")]
    pub async fn add_member(&mut self, id: u64, addr: &str) -> Result<(), EngineError> {
        self.require_admin().await?;
        Ok(self.cluster()?.add_member(id, addr).await?)
    }

    /// Remove node `id` from the cluster's voters.
    ///
    /// # Errors
    ///
    /// As for [`Session::add_member`].
    #[cfg(feature = "cluster")]
    pub async fn remove_member(&mut self, id: u64) -> Result<(), EngineError> {
        self.require_admin().await?;
        Ok(self.cluster()?.remove_member(id).await?)
    }

    /// Hand the cluster's leadership to node `to`.
    ///
    /// # Errors
    ///
    /// As for [`Session::add_member`].
    #[cfg(feature = "cluster")]
    pub async fn transfer_leadership(&mut self, to: u64) -> Result<(), EngineError> {
        self.require_admin().await?;
        Ok(self.cluster()?.transfer_leadership(to).await?)
    }

    /// Grant `privilege` to `principal`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`] without `admin`, or a storage error.
    pub async fn grant(
        &mut self,
        principal: &EntityUid,
        privilege: Privilege,
    ) -> Result<(), EngineError> {
        self.run(Kind::Admin, async |step: &Step<'_>| {
            privileges::grant(step.txn, principal, privilege).await
        })
        .await
    }

    /// Revoke `privilege` from `principal`; return whether it was held.
    ///
    /// # Errors
    ///
    /// As for [`Session::grant`], or [`EngineError::LastAdmin`] if this
    /// would revoke `admin` from the last principal holding it.
    pub async fn revoke(
        &mut self,
        principal: &EntityUid,
        privilege: Privilege,
    ) -> Result<bool, EngineError> {
        self.run(Kind::Admin, async |step: &Step<'_>| {
            privileges::revoke(step.txn, principal, privilege).await
        })
        .await
    }

    /// Propose a policy change; return the proposal's ID (ADR-0030).
    ///
    /// # Errors
    ///
    /// As for [`Session::grant`].
    pub async fn submit_policies(
        &mut self,
        document: &PolicyDocument,
        invariants: &str,
    ) -> Result<i64, EngineError> {
        let proposer = format!("{}", self.context.principal);
        let result = self
            .run(Kind::Admin, async |step: &Step<'_>| {
                submit_proposal(step.txn, document, invariants, &proposer).await
            })
            .await;
        record_proposal("submit", &result);
        result
    }

    /// Propose a policy set for namespace `scope` (ADR-0042); return the
    /// proposal's ID, to verify and activate like any proposal.
    ///
    /// The set is combined with the database-wide baseline, whose forbids
    /// still apply, and may concern only the namespace's own entity types.
    /// The namespace's owner may propose, verify and activate its set.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`] unless the principal owns
    /// the namespace or has `admin`, or the errors of
    /// [`policy::submit_scoped_proposal`](crate::policy::submit_scoped_proposal).
    pub async fn submit_scoped_policies(
        &mut self,
        scope: &str,
        document: &PolicyDocument,
        invariants: &str,
    ) -> Result<i64, EngineError> {
        let proposer = format!("{}", self.context.principal);
        let result = self
            .run(Kind::Policy, async |step: &Step<'_>| {
                may_change(step, &[scope]).await?;
                crate::policy::submit_scoped_proposal(
                    step.txn, scope, document, invariants, &proposer,
                )
                .await
            })
            .await;
        record_proposal("submit", &result);
        result
    }

    /// Run the local gates on a proposal and record the results (ADR-0030),
    /// analysing impact with this session's context as the base request.
    ///
    /// # Errors
    ///
    /// As for [`Session::grant`].
    pub async fn verify_policies(&mut self, proposal: i64) -> Result<Vec<GateResult>, EngineError> {
        let verifier = format!("{}", self.context.principal);
        let base = self.context.clone();
        let result = self
            .run(Kind::Policy, async |step: &Step<'_>| {
                may_change_policies(step, proposal).await?;
                let catalog: Arc<dyn interlaken_ir::catalog::Catalog> = step.catalog.clone();
                verify_locally(step.txn, catalog, proposal, &verifier, &base).await
            })
            .await;
        record_proposal("verify", &result);
        result
    }

    /// Activate a verified proposal; return the new policy version
    /// (ADR-0030). With `confirm`, this session's principal confirms gates
    /// that flagged the change.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::PermissionDenied`] without `admin`, or the
    /// activation's error (see [`activate`]).
    pub async fn activate_policies(
        &mut self,
        proposal: i64,
        confirm: bool,
    ) -> Result<i64, EngineError> {
        let confirmed_by = confirm.then(|| format!("{}", self.context.principal));
        let result = self
            .run(Kind::Policy, async |step: &Step<'_>| {
                may_change_policies(step, proposal).await?;
                activate(step.txn, proposal, confirmed_by.as_deref()).await
            })
            .await;
        record_proposal("activate", &result);
        result
    }

    /// Replace the policy settings.
    ///
    /// # Errors
    ///
    /// As for [`Session::activate_policies`].
    pub async fn set_policy_settings(
        &mut self,
        settings: &PolicySettings,
    ) -> Result<(), EngineError> {
        self.run(Kind::Policy, async |step: &Step<'_>| {
            may_change(step, &[]).await?;
            set_policy_settings(step.txn, settings).await
        })
        .await
    }
}

/// Run `batches` to their end, discarding the rows, by `deadline` if any.
///
/// # Errors
///
/// Returns the execution error, or SQLSTATE `57014` past the deadline.
async fn drain(mut batches: BatchStream, deadline: Option<Instant>) -> Result<(), ExecError> {
    let run = async {
        while let Some(batch) = batches.next().await {
            batch?;
        }
        Ok(())
    };
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), run)
            .await
            .unwrap_or(Err(ExecError::QueryCanceled)),
        None => run.await,
    }
}
