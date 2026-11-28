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
//! Sizes are estimates of the heap an operator holds, measured against a
//! counting allocator (`tests/memory_estimates.rs`):
//!
//! - batches count their Arrow buffers, or, for a slice of a larger buffer
//!   (sort, aggregation and `Nest` outputs are split into slices), only the
//!   bytes of the slice's own rows ([`batch_bytes`]);
//! - values count their inline size plus every heap allocation they own,
//!   by capacity, rounded up to the chunk the allocator hands out
//!   ([`alloc_bytes`]);
//! - hash tables count their buckets and control bytes from the table's
//!   capacity, and while a table grows, the old and the new table together
//!   ([`Reservation::reserve_entry`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arrow_array::cast::AsArray;
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType as ArrowType;
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
    // A compare-exchange loop rather than `fetch_update`, which is deprecated
    // in favour of `try_update`, which is newer than the MSRV.
    let mut current = used.load(Ordering::Acquire);
    loop {
        let Some(total) = current.checked_add(bytes).filter(|&total| total <= limit) else {
            return false;
        };
        match used.compare_exchange_weak(current, total, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                peak.fetch_max(total, Ordering::AcqRel);
                return true;
            }
            Err(actual) => current = actual,
        }
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

    /// Reserve for one more entry of type `T` in a hash table holding `len`
    /// entries in room for `capacity`, plus `heap` bytes the entry owns.
    ///
    /// When the insert will grow the table, the new table is reserved on top
    /// of the old one, since both exist while entries move. Returns the old
    /// table's bytes, which the caller releases ([`Reservation::shrink`])
    /// once the entry is inserted.
    ///
    /// The tables are `std`'s (hashbrown's): a power-of-two number of
    /// buckets, at most seven eighths full, each holding a `T` and a control
    /// byte, and doubling when full. Entries are never removed from the
    /// tables operators reserve for, so a full table always grows.
    ///
    /// # Errors
    ///
    /// As for [`Reservation::grow`]; nothing is reserved then.
    pub(crate) fn reserve_entry<T>(
        &mut self,
        len: usize,
        capacity: usize,
        heap: usize,
    ) -> Result<usize, ExecError> {
        if len < capacity {
            self.grow(heap)?;
            return Ok(0);
        }
        let old = buckets(capacity);
        let new = if old == 0 { 4 } else { 2 * old };
        self.grow(table_bytes::<T>(new) + heap)?;
        Ok(table_bytes::<T>(old))
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

/// Above this many bytes, glibc's `malloc` maps pages instead of carving a chunk from its heap.
const MMAP_THRESHOLD: usize = 128 * 1024;

/// Estimate the bytes the allocator hands out for a request of `n` bytes.
///
/// The model is glibc's `malloc` on 64-bit targets: a chunk is the request
/// plus an 8-byte header, rounded up to 16 bytes, and at least 32; large
/// requests are mapped in whole pages. Small values thus cost noticeably
/// more than their length: a 13-byte string takes a 32-byte chunk.
pub(crate) fn alloc_bytes(n: usize) -> usize {
    match n {
        0 => 0,
        n if n >= MMAP_THRESHOLD => (n + 16).next_multiple_of(4096),
        n => (n + 8).next_multiple_of(16).max(32),
    }
}

/// Estimate the heap of a `Vec<T>` with room for `capacity` elements.
pub(crate) fn vec_bytes<T>(capacity: usize) -> usize {
    alloc_bytes(capacity * std::mem::size_of::<T>())
}

/// Return the extra heap the next push onto a `Vec<T>` holding `len` of `capacity` allocates.
///
/// A full vector doubles, to at least four elements (`std`'s growth rule
/// for elements of up to 1 KiB).
pub(crate) fn push_bytes<T>(len: usize, capacity: usize) -> usize {
    if len < capacity {
        return 0;
    }
    let grown = (2 * capacity).max(4).max(len + 1);
    vec_bytes::<T>(grown) - vec_bytes::<T>(capacity)
}

/// Return the buckets of a hash table with room for `capacity` entries.
fn buckets(capacity: usize) -> usize {
    match capacity {
        0 => 0,
        // Tables of fewer than eight buckets fill all but one of them.
        c if c < 8 => c + 1,
        c => c / 7 * 8,
    }
}

/// Estimate the heap of a hash table of `buckets` buckets holding entries of type `T`.
///
/// One allocation holds the entries, aligned to 16 bytes, then a control
/// byte per bucket and one group (16 bytes) of trailing control bytes.
fn table_bytes<T>(buckets: usize) -> usize {
    if buckets == 0 {
        return 0;
    }
    let entries = (buckets * std::mem::size_of::<T>()).next_multiple_of(16);
    alloc_bytes(entries + buckets + 16)
}

/// Estimate the heap of a hash table of entries of type `T` with room for `capacity` entries.
pub(crate) fn table_capacity_bytes<T>(capacity: usize) -> usize {
    table_bytes::<T>(buckets(capacity))
}

/// Estimate the memory a batch holds: its Arrow buffers, or for a slice of
/// larger buffers, only its own rows' share (see [`array_bytes`]).
///
/// Slices of one batch then add up to that batch, so an operator buffering
/// another blocking operator's sliced output counts it once, not once per
/// slice.
pub(crate) fn batch_bytes(batch: &RecordBatch) -> usize {
    batch
        .columns()
        .iter()
        .map(|c| array_bytes(c.as_ref()))
        .sum()
}

/// Estimate the memory an array holds.
///
/// An array whose buffers are at most twice its own rows' bytes (plus a
/// little fixed overhead) counts its buffers whole, including spare
/// capacity a builder left. One sliced from much larger buffers counts only
/// its own rows: the buffers are shared with the other slices, and whoever
/// holds all of them holds the buffers once.
pub(crate) fn array_bytes(array: &dyn Array) -> usize {
    /// Fixed overhead below which the whole buffers are counted regardless.
    const SLACK: usize = 1024;
    let whole = array.get_array_memory_size();
    let own = own_bytes(array).unwrap_or(whole);
    if whole <= 2 * own + SLACK { whole } else { own }
}

/// Return the bytes of an array's own rows, following list offsets into their values.
///
/// Arrow's slice size counts a sliced list's whole child; here only the
/// child range the slice's offsets cover is counted. `None` if Arrow can't
/// size the array.
fn own_bytes(array: &dyn Array) -> Option<usize> {
    let nulls = array.nulls().map_or(0, |n| n.len().div_ceil(8));
    match array.data_type() {
        ArrowType::List(_) => {
            let list = array.as_list::<i32>();
            let offsets = list.value_offsets();
            let (start, end) = (offsets[0], offsets[offsets.len() - 1]);
            let values = list.values().slice(
                usize::try_from(start).ok()?,
                usize::try_from(end - start).ok()?,
            );
            Some(nulls + std::mem::size_of_val(offsets) + own_bytes(values.as_ref())?)
        }
        ArrowType::Struct(_) => {
            let columns = array.as_struct().columns();
            let fields = columns
                .iter()
                .map(|c| own_bytes(c.as_ref()))
                .sum::<Option<usize>>()?;
            Some(nulls + fields)
        }
        _ => array.to_data().get_slice_memory_size().ok(),
    }
}

/// Estimate the heap a value owns (not its inline size).
pub(crate) fn datum_heap_bytes(datum: &Datum) -> usize {
    match datum {
        Datum::Text(s) | Datum::Jsonb(s) => alloc_bytes(s.capacity()),
        Datum::Bytea(b) => alloc_bytes(b.capacity()),
        Datum::Array(items) | Datum::Record(items) => row_heap_bytes(items),
        _ => 0,
    }
}

/// Estimate the heap a vector of values owns: its buffer and every value's heap.
pub(crate) fn row_heap_bytes(row: &Vec<Datum>) -> usize {
    vec_bytes::<Datum>(row.capacity()) + row.iter().map(datum_heap_bytes).sum::<usize>()
}

/// Estimate the heap a hash key owns.
pub(crate) fn key_bytes(key: &RowKey) -> usize {
    row_heap_bytes(&key.0)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[test]
    /// Assert the bucket model matches `std`'s tables as they grow.
    fn table_model_follows_std_growth() {
        let mut set: HashSet<u64> = HashSet::new();
        let mut expected = 0;
        for i in 0..100_000u64 {
            if set.len() == set.capacity() {
                expected = if expected == 0 { 4 } else { 2 * expected };
            }
            set.insert(i);
            assert_eq!(buckets(set.capacity()), expected, "after {i}");
        }
        let map: HashMap<u64, u64> = HashMap::with_capacity(1000);
        assert_eq!(buckets(map.capacity()), 2048);
    }

    #[test]
    /// Assert pushes allocate as the growth model says.
    fn push_model_follows_std_growth() {
        let mut v: Vec<u32> = Vec::new();
        for i in 0..10_000 {
            let (len, capacity) = (v.len(), v.capacity());
            let before = vec_bytes::<u32>(capacity);
            v.push(i);
            assert_eq!(
                vec_bytes::<u32>(v.capacity()) - before,
                push_bytes::<u32>(len, capacity),
                "push {i}"
            );
        }
    }

    #[test]
    /// Assert slices of one array add up to about its rows' bytes, and an unsliced array counts its buffers.
    fn slices_count_their_own_rows() {
        use std::sync::Arc;

        use arrow_array::{ArrayRef, Int64Array, ListArray, StringArray, StructArray};
        use arrow_buffer::OffsetBuffer;
        use arrow_schema::{Field, Fields};

        let n = 100_000;
        let ids: ArrayRef = Arc::new(Int64Array::from_iter_values(0..n));
        let names: ArrayRef = Arc::new(StringArray::from_iter_values(
            (0..n).map(|i| format!("row number {i}")),
        ));
        let fields = Fields::from(vec![
            Field::new("id", arrow_schema::DataType::Int64, false),
            Field::new("name", arrow_schema::DataType::Utf8, false),
        ]);
        let records = StructArray::new(fields.clone(), vec![ids.clone(), names.clone()], None);
        // Lists of ten records each.
        let lists: ArrayRef = Arc::new(ListArray::new(
            Arc::new(Field::new(
                "item",
                arrow_schema::DataType::Struct(fields),
                false,
            )),
            OffsetBuffer::from_lengths(std::iter::repeat_n(10, usize::try_from(n).unwrap() / 10)),
            Arc::new(records),
            None,
        ));
        for array in [ids, names, lists] {
            let whole = array_bytes(array.as_ref());
            assert_eq!(whole, array.get_array_memory_size());
            let len = array.len();
            let slices: usize = (0..len)
                .step_by(1024)
                .map(|s| array_bytes(array.slice(s, 1024.min(len - s)).as_ref()))
                .sum();
            // Slices share the buffers but not a builder's spare capacity.
            let rows = own_bytes(array.as_ref()).unwrap();
            assert!(
                slices <= whole && slices >= rows - rows / 20,
                "{}: slices {slices}, whole {whole}, rows {rows}",
                array.data_type()
            );
        }
    }

    #[test]
    /// Assert value estimates count capacities and allocator chunks.
    fn values_count_capacity_and_chunks() {
        assert_eq!(alloc_bytes(0), 0);
        assert_eq!(alloc_bytes(1), 32);
        assert_eq!(alloc_bytes(24), 32);
        assert_eq!(alloc_bytes(25), 48);
        assert_eq!(alloc_bytes(MMAP_THRESHOLD), MMAP_THRESHOLD + 4096);
        let mut s = String::with_capacity(100);
        s.push('x');
        assert_eq!(datum_heap_bytes(&Datum::Text(s)), alloc_bytes(100));
        let key = RowKey(vec![Datum::Int8(1), Datum::Text("abc".into())]);
        assert_eq!(key_bytes(&key), alloc_bytes(64) + alloc_bytes(3));
        assert_eq!(datum_heap_bytes(&Datum::Int8(7)), 0);
    }
}
