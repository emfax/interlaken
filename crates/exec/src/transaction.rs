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

use std::sync::Arc;

use arrow_array::RecordBatch;
use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::{Datum, ExecOptions};
use interlaken_storage::{SavepointId, Txn};

use crate::error::ExecError;
use crate::exec::{ExecContext, collect};
use crate::memory::{DEFAULT_QUERY_BUDGET, MemoryPool, QueryMemory};
use crate::physical::Physical;

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
        if self.aborted {
            return Err(ExecError::TransactionAborted);
        }
        self.txn.next_statement();
        let savepoint = self.txn.savepoint();
        let ctx = ExecContext::new(self.txn.clone(), self.catalog.clone(), params, self.options)
            .with_memory(QueryMemory::new(self.query_budget, self.pool.clone()));
        let result = collect(plan, &ctx).await;
        drop(ctx);
        match result {
            Ok(batches) => {
                self.txn.release_savepoint(savepoint)?;
                Ok(batches)
            }
            Err(e) => {
                self.txn.rollback_to(savepoint)?;
                self.txn.release_savepoint(savepoint)?;
                self.aborted = true;
                Err(e)
            }
        }
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
