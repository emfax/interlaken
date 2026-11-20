//! Running statements in a transaction with Postgres semantics (ADR-0026).
//!
//! Each statement starts with `next_statement()`, so it sees every earlier
//! statement's writes but none of its own, and runs under an implicit
//! savepoint. A failing statement is rolled back to that savepoint, leaving
//! earlier statements' writes intact, and the transaction becomes aborted:
//! every later statement fails with `25P02` until the client rolls back to
//! a savepoint taken before the failure, or ends the transaction. A commit
//! of an aborted transaction rolls it back. Serialization failures at
//! commit are returned (`40001`) and never retried here.
//!
//! [`Transaction::statement`] runs a statement given as a closure. A
//! statement whose results outlive one call, such as a result stream the
//! client pulls from, uses the lower-level [`Transaction::begin_statement`]
//! and ends the [`Statement`] it returns with
//! [`Transaction::release_statement`], [`Transaction::rollback_statement`]
//! or [`Transaction::cancel_statement`].

use std::sync::Arc;

use arrow_array::RecordBatch;
use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::{Datum, ExecOptions};
use interlaken_storage::{SavepointId, Txn};

use crate::error::ExecError;
use crate::exec::{ExecContext, collect};
use crate::memory::{DEFAULT_QUERY_BUDGET, MemoryPool, QueryMemory};
use crate::physical::Physical;

/// A statement in progress, begun by [`Transaction::begin_statement`].
///
/// It must be ended by one of [`Transaction::release_statement`] (keep its
/// writes), [`Transaction::rollback_statement`] (undo them and abort the
/// transaction) or [`Transaction::cancel_statement`] (undo them only).
/// Dropping it without ending it leaves its savepoint open, so its writes
/// stay until the transaction ends or a rollback to an earlier savepoint.
#[derive(Debug)]
#[must_use = "a statement must be released, rolled back or cancelled"]
pub struct Statement {
    /// The statement's implicit savepoint.
    savepoint: SavepointId,
}

/// A transaction that runs statements.
pub struct Transaction {
    /// The storage transaction.
    txn: Txn,
    /// The catalog plans were built against.
    catalog: Arc<dyn Catalog>,
    /// Evaluation settings.
    options: ExecOptions,
    /// Whether a statement failed since the last rollback to a savepoint.
    aborted: bool,
    /// Each statement's memory budget in bytes (ADR-0027).
    query_budget: usize,
    /// The node-wide pool statements draw on, if any.
    pool: Option<Arc<MemoryPool>>,
}

impl Transaction {
    /// Wrap a storage transaction.
    #[must_use]
    pub fn new(txn: Txn, catalog: Arc<dyn Catalog>, options: ExecOptions) -> Self {
        Self {
            txn,
            catalog,
            options,
            aborted: false,
            query_budget: DEFAULT_QUERY_BUDGET,
            pool: None,
        }
    }

    /// Give each statement a memory budget of `query_budget` bytes, drawn from `pool` if given.
    #[must_use]
    pub fn with_memory(self, query_budget: usize, pool: Option<Arc<MemoryPool>>) -> Self {
        Self {
            query_budget,
            pool,
            ..self
        }
    }

    /// Return the storage transaction.
    #[must_use]
    pub fn txn(&self) -> &Txn {
        &self.txn
    }

    /// Return whether a failed statement left the transaction aborted.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.aborted
    }

    /// Replace the catalog later statements are planned and run against,
    /// as after DDL earlier in the transaction.
    pub fn set_catalog(&mut self, catalog: Arc<dyn Catalog>) {
        self.catalog = catalog;
    }

    /// Return the catalog statements run against.
    #[must_use]
    pub fn catalog(&self) -> &Arc<dyn Catalog> {
        &self.catalog
    }

    /// Return a context for running a statement's plan with `params`, with
    /// this transaction's catalog, settings and memory budget.
    #[must_use]
    pub fn context(&self, params: Vec<Datum>) -> ExecContext {
        ExecContext::new(self.txn.clone(), self.catalog.clone(), params, self.options)
            .with_memory(QueryMemory::new(self.query_budget, self.pool.clone()))
    }

    /// Run one statement and return its result rows (or `RETURNING` rows).
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::TransactionAborted`] if an earlier statement
    /// failed, or the statement's own error, after rolling its writes back
    /// and marking the transaction aborted.
    pub async fn execute(
        &mut self,
        plan: &Physical,
        params: Vec<Datum>,
    ) -> Result<Vec<RecordBatch>, ExecError> {
        let ctx = self.context(params);
        self.statement(async move |_| collect(plan, &ctx).await)
            .await
    }

    /// Run `body` as one statement: it sees every earlier statement's
    /// writes but none of its own reads' later writes, and its writes are
    /// rolled back if it fails.
    ///
    /// This is how statements other than plans (DDL, policy changes) get
    /// the same atomicity, and how a caller cancels a statement: `body`
    /// returns an error (such as [`ExecError::QueryCanceled`]) after
    /// dropping whatever it was running, and the statement is undone.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::TransactionAborted`] (converted) if an earlier
    /// statement failed, or `body`'s error, after rolling its writes back
    /// and marking the transaction aborted.
    pub async fn statement<T, E>(
        &mut self,
        body: impl AsyncFnOnce(&Txn) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ExecError>,
    {
        let statement = self.begin_statement()?;
        match body(&self.txn).await {
            Ok(value) => {
                self.release_statement(statement)?;
                Ok(value)
            }
            Err(e) => {
                self.rollback_statement(statement)?;
                Err(e)
            }
        }
    }

    /// Begin a statement: advance to the next statement, so it sees every
    /// earlier statement's writes but not its own, and open its implicit
    /// savepoint.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::TransactionAborted`] if an earlier statement failed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(txn: &mut interlaken_exec::Transaction) -> Result<(), interlaken_exec::ExecError> {
    /// let statement = txn.begin_statement()?;
    /// // ... run plans through `txn.txn()`, pulling their streams ...
    /// txn.release_statement(statement)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn begin_statement(&mut self) -> Result<Statement, ExecError> {
        if self.aborted {
            return Err(ExecError::TransactionAborted);
        }
        self.txn.next_statement();
        Ok(Statement {
            savepoint: self.txn.savepoint(),
        })
    }

    /// End `statement` successfully, keeping its writes.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the statement's savepoint is no longer
    /// open (a rollback to an earlier savepoint closed it).
    pub fn release_statement(&mut self, statement: Statement) -> Result<(), ExecError> {
        Ok(self.txn.release_savepoint(statement.savepoint)?)
    }

    /// End `statement` as failed: undo its writes and mark the transaction
    /// aborted, so later statements fail with `25P02` until a rollback.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the statement's savepoint is no longer
    /// open; the transaction is aborted all the same.
    pub fn rollback_statement(&mut self, statement: Statement) -> Result<(), ExecError> {
        self.aborted = true;
        self.txn.rollback_to(statement.savepoint)?;
        Ok(self.txn.release_savepoint(statement.savepoint)?)
    }

    /// End `statement` as cancelled by its caller, who stopped reading its
    /// results: undo its writes, as if it had never run, without aborting
    /// the transaction.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the statement's savepoint is no longer
    /// open, after marking the transaction aborted, since the statement's
    /// writes could not be undone.
    pub fn cancel_statement(&mut self, statement: Statement) -> Result<(), ExecError> {
        let undone = self
            .txn
            .rollback_to(statement.savepoint)
            .and_then(|()| self.txn.release_savepoint(statement.savepoint));
        if undone.is_err() {
            self.aborted = true;
        }
        Ok(undone?)
    }

    /// Open a savepoint at the current state.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::TransactionAborted`] if the transaction is aborted.
    pub fn savepoint(&self) -> Result<SavepointId, ExecError> {
        if self.aborted {
            return Err(ExecError::TransactionAborted);
        }
        Ok(self.txn.savepoint())
    }

    /// Undo everything since `savepoint` and leave the aborted state.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the savepoint is not open.
    pub fn rollback_to(&mut self, savepoint: SavepointId) -> Result<(), ExecError> {
        self.txn.rollback_to(savepoint)?;
        self.aborted = false;
        Ok(())
    }

    /// Release `savepoint`, keeping its writes.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::TransactionAborted`] if the transaction is
    /// aborted, or a storage error if the savepoint is not open.
    pub fn release(&self, savepoint: SavepointId) -> Result<(), ExecError> {
        if self.aborted {
            return Err(ExecError::TransactionAborted);
        }
        Ok(self.txn.release_savepoint(savepoint)?)
    }

    /// Commit, or roll back if the transaction is aborted.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::TransactionAborted`] after rolling back an aborted
    /// transaction, or the storage error of a failed commit (a serialization
    /// failure is SQLSTATE `40001`; the caller decides whether to retry).
    pub async fn commit(self) -> Result<(), ExecError> {
        if self.aborted {
            self.txn.abort();
            return Err(ExecError::TransactionAborted);
        }
        self.txn.commit().await?;
        Ok(())
    }

    /// Roll the whole transaction back.
    pub fn rollback(self) {
        self.txn.abort();
    }
}
