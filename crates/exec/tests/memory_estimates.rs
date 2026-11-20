//! Memory reservations against the heap (ADR-0027, backlog `memory-estimates`
//! and `slice-memory-accounting`).
//!
//! A counting global allocator (`interlaken-test-alloc`) tracks the heap
//! this process holds, in the chunks glibc's `malloc` hands out, which is
//! what the resident set pays. Each blocking operator runs over
//! [`ROWS`] rows, under a `count(*)` so its output streams away, and its
//! peak reservation is compared with the peak heap growth during the
//! query, less that of a plain scan of the same table. They must agree
//! within [`FACTOR`] either way.
//!
//! Everything runs in one test so that no other test allocates meanwhile.

use std::sync::Arc;

use futures::StreamExt;
use interlaken_exec::{ExecContext, ExecError, QueryMemory, execute, plan};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::ExecOptions;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan};
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::{
    Durability, FjallStorage, Storage, StorageOptions, Txn, TxnOptions, rows,
};
use interlaken_test_alloc::{Counting, peak, reset_peak};

/// Rows in `t`.
const ROWS: i64 = 100_000;

/// How far a reservation may be from the heap it stands for, either way.
///
/// The goal is 1.3 (backlog `memory-estimates`); the estimates come within
/// 1.1, and the tighter bound catches more mistakes, such as a hash table's
/// old buckets never being released.
const FACTOR: f64 = 1.2;

#[global_allocator]
/// The process's allocator, counting the heap held.
static ALLOCATOR: Counting = Counting;

/// The IR catalog: `t(id int8 primary key, k int8, a int4, b text)`.
fn ir_catalog() -> Arc<dyn Catalog> {
    let catalog: InMemoryCatalog = parse_catalog(
        "(catalog (builtins)
           (table t :id 100 (column id (not-null int8)) (column k int8) (column a int4) (column b text)
             (primary-key id)))",
    )
    .unwrap();
    Arc::new(catalog)
}

/// Open storage with `t` holding [`ROWS`] rows.
///
/// Row `i` is `(i, k, i % 100, "row number k")` where `k` is a permutation
/// of `0..ROWS`, so `k` and `b` are unique and arrive out of order.
async fn storage(dir: &std::path::Path) -> FjallStorage {
    let storage = FjallStorage::open(
        dir,
        StorageOptions {
            durability: Durability::Buffered,
            gc_interval: None,
            ..StorageOptions::default()
        },
    )
    .unwrap();
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let def = catalog::create_table(
        &txn,
        TableSpec {
            name: "t".into(),
            columns: vec![
                ColumnDef::new("id", ColumnType::Int8).not_null(),
                ColumnDef::new("k", ColumnType::Int8),
                ColumnDef::new("a", ColumnType::Int4),
                ColumnDef::new("b", ColumnType::Text),
            ],
            primary_key: vec!["id".into()],
        },
    )
    .await
    .unwrap();
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(1024) {
        let batch = chunk
            .iter()
            .map(|&i| {
                let k = (i * 7919) % ROWS;
                vec![
                    Datum::Int8(i),
                    Datum::Int8(k),
                    Datum::Int4(i32::try_from(i % 100).unwrap()),
                    Datum::Text(format!("row number {k}")),
                ]
            })
            .collect();
        rows::insert_many(&txn, &def, batch).await.unwrap();
    }
    txn.commit().await.unwrap();
    storage
}

/// One query's peak reservation and peak heap growth.
struct Measured {
    /// The most bytes the query reserved at once.
    reserved: usize,
    /// The most the heap grew during the query.
    heap: usize,
    /// Rows out.
    rows: usize,
}

/// Run `text` with `budget`, consuming its batches as they come.
async fn measure(txn: &Txn, text: &str, budget: usize) -> Result<Measured, ExecError> {
    let catalog = ir_catalog();
    let resolved = resolve(&parse_plan(text).unwrap(), catalog.as_ref())
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .plan;
    let node = plan(&resolved, catalog.as_ref()).unwrap();
    let memory = QueryMemory::new(budget, None);
    let ctx = ExecContext::new(txn.clone(), catalog, Vec::new(), ExecOptions::default())
        .with_memory(memory.clone());
    let before = reset_peak();
    let mut stream = execute(&node, &ctx);
    let mut rows = 0;
    let mut result = Ok(());
    while let Some(batch) = stream.next().await {
        match batch {
            Ok(b) => rows += b.num_rows(),
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    drop(stream);
    let heap = peak().saturating_sub(before);
    assert_eq!(memory.used(), 0, "{text} kept memory");
    result.map(|()| Measured {
        reserved: memory.peak(),
        heap,
        rows,
    })
}

/// Return `text` under a `count(*)`, so its output streams away.
fn counted(text: &str) -> String {
    format!("(plan (aggregate {text} (keys) (aggs (agg count))))")
}

/// The blocking operators measured, with a name for messages.
const CASES: [(&str, &str); 13] = [
    (
        "hash join on a text key",
        "(join inner (get t :as x) (get t :as y) (= x.b y.b))",
    ),
    (
        "hash join on an integer key",
        "(join inner (get t :as x) (get t :as y) (= x.k y.k))",
    ),
    (
        // The build side's batches and their concatenation, not the index, are the peak.
        "hash join building wide rows on 100 keys",
        "(join inner (filter (get t :as x) (< x.id 10)) (get t :as y) (= x.a y.a))",
    ),
    ("sort on a text key", "(sort (get t) (asc b))"),
    ("sort on two keys", "(sort (get t) (asc a) (desc k))"),
    (
        "aggregation into 100,000 groups",
        "(aggregate (get t) (keys k) (aggs (agg sum id) (agg max b)))",
    ),
    (
        "aggregation into 100 groups keeping tuples",
        "(aggregate (get t) (keys a) (aggs (agg count :distinct true k)))",
    ),
    ("distinct", "(distinct (project (get t) b))"),
    (
        "intersect",
        "(intersect (project (get t) b) (project (get t) b) :all true)",
    ),
    ("nest", "(nest (get t) (keys a) (fields id b) :as rows)"),
    ("sort over a sort", SORT_OVER_SORT),
    ("hash join building from a sort", JOIN_OVER_SORT),
    ("sort over a nest of 100,000 groups", SORT_OVER_NEST),
];

/// A sort buffering another sort's output.
const SORT_OVER_SORT: &str = "(sort (sort (get t) (asc b)) (desc k))";

/// A hash join building from a sort's output.
const JOIN_OVER_SORT: &str =
    "(join inner (get t :as x) (sort (get t :as y) (asc y.k)) (= x.b y.b))";

/// A sort buffering a `Nest`'s output, which is sliced from one batch.
const SORT_OVER_NEST: &str = "(sort (nest (get t) (keys k) (fields id b) :as rows) (desc k))";

/// A budget the plans buffering another blocking operator's output fit,
/// but not when every slice of that output counted its whole batch (about
/// a hundred times over at [`ROWS`] rows).
const SLICED_BUDGET: usize = 64 * 1024 * 1024;

#[tokio::test]
/// Assert every blocking operator's peak reservation is within [`FACTOR`]
/// of the heap it grows, and that buffering another blocking operator's
/// sliced output counts it once.
async fn reservations_match_the_heap() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    let txn = storage
        .begin(TxnOptions {
            read_only: true,
            ..TxnOptions::default()
        })
        .await
        .unwrap();

    // A plain scan, run twice so storage's caches are warm, is the baseline.
    let scan = counted("(get t)");
    measure(&txn, &scan, usize::MAX).await.unwrap();
    let baseline = measure(&txn, &scan, usize::MAX).await.unwrap();
    assert!(
        baseline.reserved < 1024,
        "a scan reserved {}",
        baseline.reserved
    );
    eprintln!("baseline scan: heap {} KiB", baseline.heap / 1024);

    let mut failures = Vec::new();
    for (name, text) in CASES {
        let m = measure(&txn, &counted(text), usize::MAX).await.unwrap();
        assert_eq!(m.rows, 1);
        let heap = m.heap.saturating_sub(baseline.heap).max(1);
        #[allow(clippy::cast_precision_loss)]
        let ratio = m.reserved as f64 / heap as f64;
        eprintln!(
            "{name}: reserved {} KiB, heap {} KiB, ratio {ratio:.2}",
            m.reserved / 1024,
            heap / 1024
        );
        if !(1.0 / FACTOR..=FACTOR).contains(&ratio) {
            failures.push(format!(
                "{name}: reserved {} for a heap of {heap} ({ratio:.2})",
                m.reserved
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");

    // Buffering a blocking operator's output (once sliced from one batch)
    // fits a budget it used to exceed.
    for text in [SORT_OVER_SORT, JOIN_OVER_SORT, SORT_OVER_NEST] {
        let m = measure(&txn, &format!("(plan {text})"), SLICED_BUDGET)
            .await
            .unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(m.rows, usize::try_from(ROWS).unwrap(), "{text}");
    }
    txn.abort();
}
