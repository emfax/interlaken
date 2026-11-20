//! Query memory budgets (ADR-0027).
//!
//! Blocking operators (hash join builds, aggregation state, sort buffers,
//! duplicate elimination, `Nest` groups, DML target keys) hold
//! [`Reservation`]s against their query's [`QueryMemory`], which may draw on
//! a node-wide [`MemoryPool`] shared by concurrent queries. Growing a
//! reservation past either limit fails the query with an error naming the
//! operator; there is no spilling. Dropping a reservation (as happens when
//! a query finishes, fails or is cancelled) returns its bytes.
//!
//! Sizes are estimates: Arrow buffer sizes for buffered batches, and an
//! approximation of heap use for row values held in hash tables.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arrow_array::RecordBatch;
use interlaken_ir::interp::{Datum, RowKey};

use crate::error::ExecError;
use crate::metrics::NodeMetrics;

/// The default per-query budget: 256 MiB.
pub const DEFAULT_QUERY_BUDGET: usize = 256 * 1024 * 1024;

/// Which limit an allocation crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryScope {
    /// The query's own budget.
    Query,
    /// The node-wide cap shared by concurrent queries.
    Node,
}

impl std::fmt::Display for MemoryScope {
    /// Render the scope as a noun phrase.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Query => "query memory budget",
            Self::Node => "node memory limit",
        })
    }
}

/// Add `bytes` to `used` unless the total would exceed `limit`, raising `peak` to the new total.
fn try_add_tracked(used: &AtomicUsize, peak: &AtomicUsize, limit: usize, bytes: usize) -> bool {
    match used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_add(bytes).filter(|&total| total <= limit)
    }) {
        Ok(before) => {
            peak.fetch_max(before + bytes, Ordering::AcqRel);
            true
        }
        Err(_) => false,
    }
}

/// A node-wide memory cap shared by every query that draws on it.
#[derive(Debug)]
pub struct MemoryPool {
    /// The cap in bytes.
    limit: usize,
    /// Bytes currently reserved by all queries.
    used: AtomicUsize,
    /// The most bytes reserved at once since creation or the last [`MemoryPool::reset_peak`].
    peak: AtomicUsize,
}

impl MemoryPool {
    /// Construct a pool capped at `limit` bytes.
    #[must_use]
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }

    /// Return the bytes currently reserved by all queries.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// Return the cap in bytes.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Return the most bytes reserved at once since the pool was created
    /// or its peak was last reset.
    #[must_use]
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    /// Restart peak tracking from the bytes reserved now.
    pub fn reset_peak(&self) {
        self.peak.store(self.used(), Ordering::Release);
    }
}

/// One query's memory budget, optionally drawing on a node-wide pool.
#[derive(Debug)]
pub struct QueryMemory {
    /// The budget in bytes.
    limit: usize,
    /// Bytes currently reserved by this query's operators.
    used: AtomicUsize,
    /// The most bytes this query has reserved at once.
    peak: AtomicUsize,
    /// The node-wide pool, if any.
    pool: Option<Arc<MemoryPool>>,
}

impl QueryMemory {
    /// Construct a budget of `limit` bytes, drawing on `pool` if given.
    #[must_use]
    pub fn new(limit: usize, pool: Option<Arc<MemoryPool>>) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            pool,
        })
    }

    /// Return the bytes currently reserved by this query.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// Return the most bytes this query has reserved at once.
    #[must_use]
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    /// Reserve `bytes` more, or report which limit that would cross.
    fn acquire(&self, bytes: usize) -> Result<(), MemoryScope> {
        if !try_add_tracked(&self.used, &self.peak, self.limit, bytes) {
            return Err(MemoryScope::Query);
        }
        if let Some(pool) = &self.pool
            && !try_add_tracked(&pool.used, &pool.peak, pool.limit, bytes)
        {
            self.used.fetch_sub(bytes, Ordering::AcqRel);
            return Err(MemoryScope::Node);
        }
        Ok(())
    }

    /// Return `bytes` to the budget (and the pool).
    fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
        if let Some(pool) = &self.pool {
            pool.used.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
}

/// Memory held by one operator; returned to the budget when dropped.
pub(crate) struct Reservation {
    /// The query's budget.
    memory: Arc<QueryMemory>,
    /// The operator, for error messages.
    operator: &'static str,
    /// Bytes currently held.
    bytes: usize,
    /// The operator's metrics, which record its peak.
    metrics: Option<Arc<NodeMetrics>>,
}

impl Reservation {
    /// Construct an empty reservation for `operator`.
    pub(crate) fn new(
        memory: Arc<QueryMemory>,
        operator: &'static str,
        metrics: Option<Arc<NodeMetrics>>,
    ) -> Self {
        Self {
            memory,
            operator,
            bytes: 0,
            metrics,
        }
    }

    /// Hold `bytes` more.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::MemoryExceeded`] if the query's budget or the
    /// node's pool can't supply them; the reservation is then unchanged.
    pub(crate) fn grow(&mut self, bytes: usize) -> Result<(), ExecError> {
        if bytes == 0 {
            return Ok(());
        }
        self.memory
            .acquire(bytes)
            .map_err(|scope| ExecError::MemoryExceeded {
                operator: self.operator,
                scope,
            })?;
        self.bytes += bytes;
        if let Some(metrics) = &self.metrics {
            metrics.record_memory(self.bytes);
        }
        Ok(())
    }

    /// Hold `bytes` fewer (at most what is held).
    pub(crate) fn shrink(&mut self, bytes: usize) {
        let bytes = bytes.min(self.bytes);
        self.memory.release(bytes);
        self.bytes -= bytes;
    }

    /// Hold exactly `bytes`, growing or shrinking as needed.
    ///
    /// # Errors
    ///
    /// As for [`Reservation::grow`].
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), ExecError> {
        if bytes >= self.bytes {
            self.grow(bytes - self.bytes)
        } else {
            self.shrink(self.bytes - bytes);
            Ok(())
        }
    }
}

impl Drop for Reservation {
    /// Return every held byte to the budget.
    fn drop(&mut self) {
        self.memory.release(self.bytes);
    }
}

/// Estimate the memory a batch holds: its Arrow buffers.
pub(crate) fn batch_bytes(batch: &RecordBatch) -> usize {
    batch.get_array_memory_size()
}

/// Estimate the heap and inline memory of a value.
pub(crate) fn datum_bytes(datum: &Datum) -> usize {
    std::mem::size_of::<Datum>()
        + match datum {
            Datum::Text(s) | Datum::Jsonb(s) => s.len(),
            Datum::Bytea(b) => b.len(),
            Datum::Array(items) | Datum::Record(items) => items.iter().map(datum_bytes).sum(),
            _ => 0,
        }
}

/// Estimate the memory of a row of values held in a collection.
pub(crate) fn row_bytes(row: &[Datum]) -> usize {
    std::mem::size_of::<Vec<Datum>>() + row.iter().map(datum_bytes).sum::<usize>()
}

/// Estimate the memory of a hash table entry keyed by `key` with a value of `value` bytes.
pub(crate) fn entry_bytes(key: &RowKey, value: usize) -> usize {
    // Keys are stored once; hash tables carry roughly a word of overhead per entry.
    row_bytes(&key.0) + value + 2 * std::mem::size_of::<usize>()
}
