//! Memory budgets (ADR-0027) and `EXPLAIN ANALYZE` metrics (ADR-0028).
//!
//! Blocking operators fail cleanly, naming themselves, when they outgrow
//! the query's budget or the node's pool; streaming operators run in a
//! small budget whatever the input size; every reservation is returned
//! when a query finishes, fails or is cancelled. `EXPLAIN ANALYZE` reports
//! rows, batches, loops, storage calls and peak memory per operator.

use std::sync::Arc;

use futures::StreamExt;
use interlaken_exec::{
    ExecContext, ExecError, ExplainOptions, MemoryPool, MemoryScope, Metrics, Op, Physical,
    QueryMemory, Transaction, collect, execute, explain, explain_analyze, plan,
};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::ExecOptions;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan};
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum as Stored};
use interlaken_storage::{
    Durability, FjallStorage, Storage, StorageOptions, Txn, TxnOptions, rows,
};

/// Rows in `t`.
const ROWS: i64 = 5000;

/// A budget far below what any blocking operator needs for `t`.
const SMALL: usize = 64 * 1024;

/// The IR catalog: `t(id int8 primary key, a int4, b text)`.
fn ir_catalog() -> Arc<dyn Catalog> {
    let catalog: InMemoryCatalog = parse_catalog(
        "(catalog (builtins)
           (table t :id 100 (column id int8) (column a (option int4)) (column b (option text)) (primary-key id)))",
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

/// Open storage with `t` holding [`ROWS`] rows: (i, i % 7, "row number i").
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
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("a", ColumnType::Int4),
            ColumnDef::new("b", ColumnType::Text),
        ],
        primary_key: vec!["id".into()],
    };
    let table = catalog::create_table(&txn, spec).await.unwrap();
    for i in 0..ROWS {
        let row = vec![
            Stored::Int8(i),
            Stored::Int4(i32::try_from(i % 7).unwrap()),
            Stored::Text(format!("row number {i}")),
        ];
        rows::insert(&txn, &table, row).await.unwrap();
    }
    txn.commit().await.unwrap();
    storage
}

/// Return a context over `txn` with `memory`.
fn context(txn: &Txn, memory: Arc<QueryMemory>) -> ExecContext {
    ExecContext::new(
        txn.clone(),
        ir_catalog(),
        Vec::new(),
        ExecOptions::default(),
    )
    .with_memory(memory)
}

/// Count the rows of `batches`.
fn count(batches: &[arrow_array::RecordBatch]) -> usize {
    batches.iter().map(arrow_array::RecordBatch::num_rows).sum()
}

/// Queries whose blocking operator holds most of `t`, with that operator's name.
const BLOCKING: [(&str, &str); 7] = [
    ("(plan (sort (get t) (asc b)))", "Sort"),
    (
        "(plan (join inner (get t :as x) (get t :as y) (= x.b y.b)))",
        "HashJoin",
    ),
    (
        "(plan (aggregate (get t) (keys id) (aggs (agg max b))))",
        "HashAggregate",
    ),
    ("(plan (distinct (project (get t) b)))", "Distinct"),
    (
        "(plan (union (project (get t) b) (project (get t) b)))",
        "SetOp",
    ),
    (
        "(plan (nest (get t) (keys a) (fields id b) :as rows))",
        "Nest",
    ),
    ("(plan (update t (get t) (set a 1)))", "ModifyTable"),
];

/// Return the ids of every table scan in `node`.
fn scans(node: &Physical) -> Vec<usize> {
    let mut out: Vec<usize> = node.op.inputs().into_iter().flat_map(scans).collect();
    if matches!(node.op, Op::TableScan { .. }) {
        out.push(node.id);
    }
    out
}

#[tokio::test]
/// Assert each blocking operator fails cleanly at a small budget, naming itself, and releases everything.
async fn blocking_operators_fail_at_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    for (text, operator) in BLOCKING {
        let node = physical(text);
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        let memory = QueryMemory::new(SMALL, None);
        let metrics = Metrics::new(node.size());
        let ctx = context(&txn, memory.clone()).with_metrics(metrics.clone());
        let err = collect(&node, &ctx).await.unwrap_err();
        // The budget stops the operator while it reads, not after it has read everything.
        for id in scans(&node) {
            let read = metrics.node(id).unwrap().snapshot().rows;
            assert!(
                read < ROWS as u64,
                "{text}: a scan read all {read} rows before failing"
            );
        }
        assert!(
            matches!(err, ExecError::MemoryExceeded { operator: o, scope: MemoryScope::Query } if o == operator),
            "{text}: {err}"
        );
        assert_eq!(err.sqlstate(), "53200");
        assert_eq!(
            err.to_string(),
            format!("{operator} exceeded the query memory budget")
        );
        assert_eq!(memory.used(), 0, "{text} kept memory after failing");

        // The default budget is ample.
        let memory = QueryMemory::new(interlaken_exec::DEFAULT_QUERY_BUDGET, None);
        let ctx = context(&txn, memory.clone());
        collect(&node, &ctx)
            .await
            .unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(memory.used(), 0, "{text} kept memory after finishing");
        txn.abort();
    }
}

#[tokio::test]
/// Assert streaming operators, and a top-N, run over the whole table in a small budget.
async fn streaming_runs_in_a_small_budget() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let ctx = context(&txn, QueryMemory::new(SMALL, None));
    let streaming =
        physical("(plan (project (filter (get t) (like b \"%7%\")) id (call upper b)))");
    let expected = (0..ROWS).filter(|i| i.to_string().contains('7')).count();
    assert_eq!(count(&collect(&streaming, &ctx).await.unwrap()), expected);
    // A top-N keeps only twice its fetch (or a batch) of rows, so it fits where a full sort fails.
    // Those are two batches of rows and their key values (a text value takes
    // a 32-byte `Datum` and a heap chunk): about 200 KiB.
    let ctx = context(&txn, QueryMemory::new(4 * SMALL, None));
    let top = physical("(plan (limit (sort (get t) (desc b)) :count 3))");
    assert_eq!(count(&collect(&top, &ctx).await.unwrap()), 3);
    let sort = physical("(plan (sort (get t) (desc b)))");
    assert!(matches!(
        collect(&sort, &ctx).await,
        Err(ExecError::MemoryExceeded {
            operator: "Sort",
            ..
        })
    ));
    txn.abort();
}

#[tokio::test]
/// Assert the node pool caps concurrent queries together, and cancelling a query returns its memory.
async fn node_pool_caps_concurrent_queries() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let sort = physical("(plan (sort (get t) (asc b)))");

    // Measure what one sort needs while sorting (its peak: the input and
    // the key values) and holds once it is producing output (the input).
    let probe = QueryMemory::new(usize::MAX, None);
    let mut stream = execute(&sort, &context(&txn, probe.clone()));
    stream.next().await.unwrap().unwrap();
    let held = probe.used();
    let peak = probe.peak();
    assert!(held > SMALL, "the sort holds {held} bytes");
    assert!(
        peak > held,
        "the sort's peak {peak} is what it holds after sorting"
    );
    drop(stream);
    assert_eq!(probe.used(), 0, "cancelling returns the sort's memory");

    // Two sorts fit their own budgets, but not the pool together: the
    // second can't reach its peak while the first holds its input.
    let pool = MemoryPool::new(peak + held / 2);
    let budget = || QueryMemory::new(peak * 2, Some(pool.clone()));
    let mut first = execute(&sort, &context(&txn, budget()));
    first.next().await.unwrap().unwrap();
    assert!(pool.used() >= held);
    let err = collect(&sort, &context(&txn, budget())).await.unwrap_err();
    assert!(
        matches!(
            err,
            ExecError::MemoryExceeded {
                operator: "Sort",
                scope: MemoryScope::Node
            }
        ),
        "{err}"
    );
    assert_eq!(err.to_string(), "Sort exceeded the node memory limit");
    // Once the first is cancelled, the second runs.
    drop(first);
    assert_eq!(pool.used(), 0);
    assert_eq!(
        count(&collect(&sort, &context(&txn, budget())).await.unwrap()),
        usize::try_from(ROWS).unwrap()
    );
    assert_eq!(pool.used(), 0);

    // A cancelled hash join returns its build side too.
    let pool = MemoryPool::new(usize::MAX);
    let join = physical("(plan (join inner (get t :as x) (get t :as y) (= x.b y.b)))");
    let memory = QueryMemory::new(usize::MAX, Some(pool.clone()));
    let mut stream = execute(&join, &context(&txn, memory));
    stream.next().await.unwrap().unwrap();
    assert!(pool.used() > 0);
    drop(stream);
    assert_eq!(pool.used(), 0);
    txn.abort();
}

#[tokio::test]
/// Assert a statement over its budget aborts the transaction like any other failure.
async fn transactions_apply_statement_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let pool = MemoryPool::new(usize::MAX);
    let mut t = Transaction::new(txn, ir_catalog(), ExecOptions::default())
        .with_memory(SMALL, Some(pool.clone()));
    let err = t
        .execute(&physical("(plan (sort (get t) (asc b)))"), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "53200");
    assert!(t.is_aborted());
    assert_eq!(pool.used(), 0);
    t.rollback();
}

/// Return the line of `text` containing `needle`.
fn line<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no {needle} in\n{text}"))
}

/// Return the value of `key=` on `line`.
fn metric(line: &str, key: &str) -> u64 {
    let start = line
        .find(&format!("{key}="))
        .unwrap_or_else(|| panic!("no {key} in {line}"))
        + key.len()
        + 1;
    line[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

#[tokio::test]
/// Assert `EXPLAIN` shows algorithms and pushdown, and `EXPLAIN ANALYZE` counts what each operator did.
async fn explain_analyze_reports_per_operator_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(dir.path()).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let ctx = context(&txn, QueryMemory::new(usize::MAX, None));

    let node = physical(
        "(plan (limit (sort (filter (get t) (and (> a 3) (like b \"%1%\"))) (asc id)) :count 5))",
    );
    assert_eq!(
        explain(&node, ExplainOptions::default()),
        "Limit count: 5\n  TopN #1.0 asc\n    Filter (like #1.2 \"%1%\")\n      TableScan t pushed: (> #1.1 (int4 3))\n"
    );
    let analyzed = explain_analyze(&node, &ctx, ExplainOptions::default())
        .await
        .unwrap();
    let pushed = (0..ROWS).filter(|i| i % 7 > 3).count() as u64;
    let matching = (0..ROWS)
        .filter(|i| i % 7 > 3 && i.to_string().contains('1'))
        .count() as u64;
    let scan = line(&analyzed, "TableScan");
    assert_eq!(metric(scan, "rows"), pushed, "{analyzed}");
    // Each storage call returns up to a batch of the rows that pass the pushed filter.
    assert_eq!(metric(scan, "storage_calls"), pushed.div_ceil(1024));
    assert!(metric(scan, "bytes_read") > 0);
    assert_eq!(metric(line(&analyzed, "Filter"), "rows"), matching);
    assert!(metric(line(&analyzed, "TopN"), "peak_memory") > 0);
    assert_eq!(metric(line(&analyzed, "Limit"), "rows"), 5);
    assert_eq!(metric(line(&analyzed, "Limit"), "loops"), 1);

    // An apply runs its inner plan once per outer row.
    let node = physical(
        "(plan (apply inner (values (columns k) (row (cast (int4 3) int8)) (row (cast (int4 5) int8)) (row (cast (int4 9) int8))) (filter (get t) (= id k))))",
    );
    let analyzed = explain_analyze(&node, &ctx, ExplainOptions::default())
        .await
        .unwrap();
    let scan = line(&analyzed, "TableScan");
    assert_eq!(metric(scan, "loops"), 3, "{analyzed}");
    assert_eq!(metric(scan, "rows"), 3);
    assert!(scan.contains("primary key = (#1.0)"), "{scan}");
    assert_eq!(metric(line(&analyzed, "Apply"), "rows"), 3);
    drop(ctx);
    txn.abort();
}
