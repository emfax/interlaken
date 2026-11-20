//! Executor performance behaviour (milestone F3): cooperative scheduling,
//! index lookup joins reading by batch and switching to hash joins, and
//! aggregates holding memory proportional to their groups.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use interlaken_exec::{
    ExecContext, ExecError, ExplainOptions, Metrics, Op, Physical, QueryMemory, collect, explain,
    explain_analyze, plan,
};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Datum, ExecOptions};
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan};
use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum as Stored, SortOrder};
use interlaken_storage::{
    Durability, FjallStorage, Storage, StorageOptions, Txn, TxnOptions, rows,
};

/// Rows in `t`.
const ROWS: i64 = 5000;

/// The IR catalog: `t(id int8 primary key, a int4, b text, r int8)`, indexed on `a` and `r`.
fn ir_catalog() -> Arc<dyn Catalog> {
    let catalog: InMemoryCatalog = parse_catalog(
        "(catalog (builtins)
           (table t :id 100 (column id (not-null int8)) (column a int4) (column b text) (column r int8)
             (primary-key id) (index t_a a) (index t_r r :unique true)))",
    )
    .unwrap();
    Arc::new(catalog)
}

/// Plan `text` against the catalog.
fn physical(text: &str) -> Physical {
    let catalog = ir_catalog();
    let resolved = resolve(&parse_plan(text).unwrap(), catalog.as_ref())
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .plan;
    plan(&resolved, catalog.as_ref()).unwrap()
}

/// Open storage with `t` holding [`ROWS`] rows: (i, i % 7, "row i", (7 i) mod ROWS).
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
    let spec = TableSpec {
        name: "t".into(),
        columns: vec![
            ColumnDef::new("id", ColumnType::Int8).not_null(),
            ColumnDef::new("a", ColumnType::Int4),
            ColumnDef::new("b", ColumnType::Text),
            ColumnDef::new("r", ColumnType::Int8),
        ],
        primary_key: vec!["id".into()],
    };
    catalog::create_table(&txn, spec).await.unwrap();
    for (name, column, unique) in [("t_a", "a", false), ("t_r", "r", true)] {
        catalog::create_index(
            &txn,
            IndexSpec {
                name: name.into(),
                table: "t".into(),
                columns: vec![(column.into(), SortOrder::ASC)],
                unique,
            },
        )
        .await
        .unwrap();
    }
    // Read the definition back, with its indexes, so inserts maintain them.
    let table = catalog::table(&txn, "t").await.unwrap().unwrap();
    let rows: Vec<Vec<Stored>> = (0..ROWS)
        .map(|i| {
            vec![
                Stored::Int8(i),
                Stored::Int4(i32::try_from(i % 7).unwrap()),
                Stored::Text(format!("row {i}")),
                Stored::Int8((7 * i) % ROWS),
            ]
        })
        .collect();
    rows::insert_many(&txn, &table, rows).await.unwrap();
    txn.commit().await.unwrap();
    storage
}

/// Return a context over `txn` with the default budget and `params`.
fn context(txn: &Txn, params: Vec<Datum>) -> ExecContext {
    ExecContext::new(txn.clone(), ir_catalog(), params, ExecOptions::default())
}

/// Return the total rows of `batches`.
fn count(batches: &[arrow_array::RecordBatch]) -> usize {
    batches.iter().map(arrow_array::RecordBatch::num_rows).sum()
}

/// Return the first operator of `node` (preorder) that `pick` accepts.
fn find<'a>(node: &'a Physical, pick: &dyn Fn(&Op) -> bool) -> Option<&'a Physical> {
    if pick(&node.op) {
        return Some(node);
    }
    node.op.inputs().into_iter().find_map(|i| find(i, pick))
}

#[test]
/// Assert hash builds, sorts and aggregations over in-memory input yield to
/// the runtime, so other tasks on the same thread keep running.
fn blocking_operators_yield_to_the_runtime() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    runtime.block_on(async {
        let storage = storage(dir.path()).await;
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        let values = Datum::Array((0..200_000).map(Datum::Int8).collect());
        // A sort's output is in memory, in many batches, so whatever consumes it never waits.
        let source = "(sort (unnest (values (columns d) (row 0)) (param 1 (array int8)) (columns x)) (desc x))";
        for text in [
            format!("(plan (aggregate {source} (keys) (aggs (agg count) (agg sum x))))"),
            format!("(plan (sort {source} (asc x)))"),
            format!("(plan (join semi (values (columns y) (row (cast 7 int8))) {source} (= y x)))"),
        ] {
            let ticks = Arc::new(AtomicU64::new(0));
            let ticker = tokio::spawn({
                let ticks = ticks.clone();
                async move {
                    loop {
                        ticks.fetch_add(1, Ordering::Relaxed);
                        tokio::task::yield_now().await;
                    }
                }
            });
            // Let the ticker start, then count only ticks during the query.
            tokio::task::yield_now().await;
            let before = ticks.load(Ordering::Relaxed);
            let node = physical(&text);
            // Slices of one sorted batch each count its whole buffer, so allow plenty.
            let ctx = context(&txn, vec![values.clone()])
                .with_memory(QueryMemory::new(usize::MAX / 2, None));
            let out = collect(&node, &ctx).await.unwrap();
            let during = ticks.load(Ordering::Relaxed) - before;
            ticker.abort();
            assert!(count(&out) > 0, "{text}");
            // Each of these consumes about 200 batches, yielding every few.
            assert!(during >= 20, "{text}: other tasks ran only {during} times");
        }
        txn.abort();
    });
}

#[tokio::test]
/// Assert a join of a few left rows with a keyed table looks them up a
/// batch at a time instead of reading the whole table.
async fn index_lookups_read_by_batch() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    for (text, using) in [
        (
            "(plan (join inner (filter (get t :as x) (< x.id 1000)) (get t :as y) (= x.r y.id)))",
            "IndexLookup inner using primary key keys: #1.3 = #2.0",
        ),
        (
            "(plan (join inner (filter (get t :as x) (< x.id 1000)) (get t :as y) (= x.id y.r)))",
            "IndexLookup inner using t_r keys: #1.0 = #2.3",
        ),
        (
            "(plan (join semi (filter (get t :as x) (< x.id 1000)) (get t :as y) (= x.a y.a)))",
            "IndexLookup semi using t_a keys: #1.1 = #2.1",
        ),
    ] {
        let node = physical(text);
        let shown = explain(&node, ExplainOptions::default());
        assert!(shown.contains(using), "{shown}");
        let metrics = Metrics::new(node.size());
        let ctx = context(&txn, Vec::new()).with_metrics(metrics.clone());
        let out = collect(&node, &ctx).await.unwrap();
        assert_eq!(count(&out), 1000, "{text}");
        let lookup = metrics.node(node.id).unwrap().snapshot();
        // One left batch of 1,000 rows: a few concurrent calls, far fewer than one per row.
        assert!(
            (1..=50).contains(&lookup.storage_calls),
            "{text}: {} storage calls",
            lookup.storage_calls
        );
        // While deciding, the right scan reads at most twice the left rows (plus a batch).
        let Op::IndexLookup { right, .. } = &node.op else {
            panic!("not an index lookup: {shown}")
        };
        let scan = find(right, &|op| matches!(op, Op::TableScan { .. })).unwrap();
        let read = metrics.node(scan.id).unwrap().snapshot().rows;
        assert!(
            read <= 2 * 1000 + 1024,
            "{text}: the right scan read {read} rows"
        );
    }
    let analyzed = explain_analyze(
        &physical(
            "(plan (join left (filter (get t :as x) (< x.id 10)) (get t :as y) (= x.r y.id)))",
        ),
        &context(&txn, Vec::new()),
        ExplainOptions::default(),
    )
    .await
    .unwrap();
    assert!(analyzed.starts_with("IndexLookup left"), "{analyzed}");
    txn.abort();
}

#[tokio::test]
/// Assert an index lookup join whose left input outgrows the right one runs
/// as a hash join instead, with the same rows.
async fn index_lookups_become_hash_joins_for_large_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    for (text, rows) in [
        // Every left row against a table no larger: hashing wins.
        (
            "(plan (join inner (get t :as x) (get t :as y) (= x.r y.id)))",
            5000,
        ),
        // A small right table (one row per value of `a` below 3).
        (
            "(plan (join left (get t :as x) (filter (get t :as y) (< y.id 3)) (= (cast x.a int8) y.id)))",
            5000,
        ),
    ] {
        let node = physical(text);
        assert!(matches!(node.op, Op::IndexLookup { .. }), "{text}");
        let metrics = Metrics::new(node.size());
        let ctx = context(&txn, Vec::new()).with_metrics(metrics.clone());
        let out = collect(&node, &ctx).await.unwrap();
        assert_eq!(count(&out), rows, "{text}");
        // A hash join reads its right input once and makes no lookups of its own.
        assert_eq!(
            metrics.node(node.id).unwrap().snapshot().storage_calls,
            0,
            "{text}"
        );
    }
    txn.abort();
}

#[tokio::test]
/// Assert accumulating aggregates hold memory for their groups only, while
/// `DISTINCT` aggregates still keep their argument tuples.
async fn aggregates_hold_memory_per_group() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let budget = || QueryMemory::new(16 * 1024, None);
    let grouped = physical(
        "(plan (aggregate (get t) (keys t.a)
           (aggs (agg count) (agg sum t.id) (agg avg t.r) (agg min t.b) (agg max t.b)
                 (agg bool_and (> t.id 3)) (agg bool_or (> t.id 3)) (agg count t.b))))",
    );
    let out = collect(&grouped, &context(&txn, Vec::new()).with_memory(budget()))
        .await
        .unwrap();
    assert_eq!(count(&out), 7);
    let distinct =
        physical("(plan (aggregate (get t) (keys t.a) (aggs (agg count t.b :distinct true))))");
    let err = collect(&distinct, &context(&txn, Vec::new()).with_memory(budget()))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ExecError::MemoryExceeded {
                operator: "HashAggregate",
                ..
            }
        ),
        "{err}"
    );
    txn.abort();
}
