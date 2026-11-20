//! Sessions: a principal's statements, in transactions (ADR-0031).

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use interlaken_exec::{
    BatchStream, ExecError, ExplainOptions, Statement, Transaction, execute, explain,
    explain_analyze,
};
use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Datum, ExecOptions};
use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_ir::{DataType, Plan, QualifiedName, SchemaVersion};
use interlaken_storage::{SavepointId, Storage, Txn, TxnOptions};
use tokio::task::JoinHandle;

use super::engine::Engine;
use super::pipeline::{Step, exec_context, parse, pin, prepare};
use super::result::QueryResult;
use super::stream::{Execution, QueryStream};
use crate::catalog::{self, ColumnSpec, EntitySpec, IndexSpec, MembershipSpec, TableSpec};
use crate::error::EngineError;
use crate::policy::{
    GateResult, PolicyDocument, PolicySettings, activate, set_policy_settings, submit_proposal,
    verify_locally,
};
use crate::privileges::{self, Privilege, Privileges};

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
}

impl Session {
    /// Construct a session with no transaction open.
    pub(crate) fn new(engine: Engine, context: SecurityContext) -> Self {
        Self {
            engine,
            context,
            statement_timeout: None,
            txn: None,
            finishing: None,
        }
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
        let txn = self.engine.inner.storage.begin(options).await?;
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
        Ok(())
    }

    /// Commit the open transaction, or roll it back if it is aborted.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::NoTransaction`] if none is open, or the
    /// commit's error (a serialization failure is SQLSTATE `40001`; the
    /// caller decides whether to retry).
    pub async fn commit(&mut self) -> Result<(), EngineError> {
        self.settle().await?;
        let open = self.txn.take().ok_or(EngineError::NoTransaction)?;
        open.exec.commit().await?;
        Ok(())
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
            Ok(value) => self.commit().await.map(|()| value),
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
                    if kind != Kind::Query && !privileges.admin {
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
                    };
                    let value = body(&step).await?;
                    if kind == Kind::Catalog {
                        // Reloading checks the changed declarations fit
                        // together; it must see this statement's writes.
                        txn.next_statement();
                        catalog::load_catalog(txn).await?;
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
    /// resolution, verification or security error, or the execution error,
    /// after undoing the statement.
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
    /// Returns [`EngineError::Syntax`] if the text doesn't parse, or a
    /// resolution, verification or security error, after undoing the
    /// statement; execution errors arrive through the stream.
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
        match parse(text) {
            Ok(plan) => self.start_stream(Ok(&plan), params).await,
            Err(e) => self.start_stream(Err(e), params).await,
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
        self.start_stream(Ok(plan), params).await
    }

    /// Begin a query statement (and an implicit transaction if none is
    /// open), plan `plan` and return the stream that executes it.
    async fn start_stream(
        &mut self,
        plan: Result<&Plan, EngineError>,
        params: Vec<Datum>,
    ) -> Result<QueryStream<'_>, EngineError> {
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
        let (catalog_dirty, policies_dirty, options) =
            (open.catalog_dirty, open.policies_dirty, open.options);
        let txn = open.exec.txn();
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
            };
            let prepared = prepare(&step, plan?).await?;
            let writes = prepared.physical.writes();
            let batches = execute(&prepared.physical, &exec_context(&step, params));
            Ok::<_, EngineError>(Execution {
                columns: prepared.columns,
                types: prepared.types,
                batches,
                writes,
            })
        };
        let started = match timeout {
            Some(limit) => tokio::time::timeout(limit, work)
                .await
                .unwrap_or(Err(EngineError::Exec(ExecError::QueryCanceled))),
            None => work.await,
        };
        match started {
            Ok(run) => Ok(QueryStream::new(self, statement, implicit, run, deadline)),
            Err(e) => {
                self.fail_statement(statement, implicit);
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
    ) -> Result<(), EngineError> {
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
            self.commit().await?;
        }
        Ok(())
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
            Ok(pin(&plan?, step.catalog.as_ref())?.0)
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
                let ctx = exec_context(step, Vec::new());
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
            catalog::create_table(step.txn, spec).await
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
            catalog::drop_table(step.txn, name).await
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
            catalog::add_column(step.txn, table, column).await
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
            catalog::create_index(step.txn, spec).await
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
            catalog::drop_index(step.txn, name).await
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
            catalog::declare_entity(step.txn, spec).await
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
            catalog::drop_entity(step.txn, name).await
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
            catalog::declare_membership(step.txn, spec).await
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
            catalog::drop_membership(step.txn, member, group).await
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
            catalog::set_function_leakproof(step.txn, &name, args, leakproof).await
        })
        .await
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
    /// As for [`Session::grant`].
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
        self.run(Kind::Admin, async |step: &Step<'_>| {
            submit_proposal(step.txn, document, invariants, &proposer).await
        })
        .await
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
        self.run(Kind::Admin, async |step: &Step<'_>| {
            let catalog: Arc<dyn interlaken_ir::catalog::Catalog> = step.catalog.clone();
            verify_locally(step.txn, catalog, proposal, &verifier, &base).await
        })
        .await
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
        self.run(Kind::Policy, async |step: &Step<'_>| {
            activate(step.txn, proposal, confirmed_by.as_deref()).await
        })
        .await
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
