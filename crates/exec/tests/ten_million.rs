//! The executor's 10-million-row criterion (`executor-design.md` success
//! criteria, ADR-0027, milestone F3), run on demand:
//!
//! ```text
//! cargo test --release -p interlaken-exec --test ten_million -- --ignored --nocapture
//! ```
//!
//! `TEN_MILLION_ROWS` overrides the row count (default 10,000,000). The
//! test loads the rows, then checks that streaming queries over all of them
//! (a filter and projection consumed batch by batch, and an aggregation
//! into a thousand groups) hold the process's resident memory flat, and
//! that a blocking query (a full sort) fails cleanly at the default query
//! budget, returning everything it reserved. It is ignored by default
//! because loading takes about a minute and a few GiB of disk.
//!
//! The same streaming criterion through the engine's public API, with
//! results streamed by `Session::query_stream` and the heap measured by a
//! counting allocator, is `crates/engine/tests/streaming_memory.rs`.

use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use interlaken_exec::{DEFAULT_QUERY_BUDGET, ExecContext, ExecError, QueryMemory, execute, plan};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::ExecOptions;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan};
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions, rows};

/// Rows per loading transaction.
const LOAD_TXN_ROWS: i64 = 100_000;

/// The most the resident set may grow during a streaming query.
const STREAMING_SLACK: u64 = 64 * 1024 * 1024;

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

/// Return a field of `/proc/self/status` in bytes, if available.
fn status_bytes(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib: u64 = status
        .lines()
        .find(|l| l.starts_with(field))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// Reset the peak resident set to the current one; return the current one, if measurable.
fn reset_peak() -> Option<u64> {
    std::fs::write("/proc/self/clear_refs", "5").ok()?;
    status_bytes("VmRSS:")
}

/// Run `text` over every row, consuming its batches as they come; return
/// (rows out, peak query reservation, growth of the peak resident set, seconds).
async fn run(
    txn: &interlaken_storage::Txn,
    text: &str,
    budget: usize,
) -> Result<(usize, usize, Option<u64>, f64), ExecError> {
    let catalog = ir_catalog();
    let resolved = resolve(&parse_plan(text).unwrap(), catalog.as_ref())
        .unwrap()
        .plan;
    let node = plan(&resolved, catalog.as_ref()).unwrap();
    let memory = QueryMemory::new(budget, None);
    let ctx = ExecContext::new(txn.clone(), catalog, Vec::new(), ExecOptions::default())
        .with_memory(memory.clone());
    let before = reset_peak();
    let started = Instant::now();
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
    let seconds = started.elapsed().as_secs_f64();
    let growth = before
        .zip(status_bytes("VmHWM:"))
        .map(|(b, p)| p.saturating_sub(b));
    assert_eq!(memory.used(), 0, "{text} kept memory");
    result.map(|()| (rows, memory.peak(), growth, seconds))
}

#[test]
#[ignore = "loads 10 million rows; run on demand with --ignored"]
/// Assert streaming queries over 10 million rows run in flat memory and a
/// blocking one fails cleanly at its budget.
fn ten_million_rows_in_bounded_memory() {
    let n: i64 = std::env::var("TEN_MILLION_ROWS")
        .ok()
        .and_then(|v| v.replace('_', "").parse().ok())
        .unwrap_or(10_000_000);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    runtime.block_on(async {
        let storage = FjallStorage::open(
            dir.path(),
            StorageOptions {
                durability: Durability::Buffered,
                gc_interval: None,
                ..StorageOptions::default()
            },
        )
        .unwrap();
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        catalog::create_table(
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
        txn.commit().await.unwrap();
        let started = Instant::now();
        let mut next = 0;
        while next < n {
            let txn = storage.begin(TxnOptions::default()).await.unwrap();
            let def = catalog::table(&txn, "t").await.unwrap().unwrap();
            let end = (next + LOAD_TXN_ROWS).min(n);
            for chunk in (next..end).collect::<Vec<_>>().chunks(1024) {
                let batch = chunk
                    .iter()
                    .map(|&i| {
                        vec![
                            Datum::Int8(i),
                            Datum::Int8(i % 1000),
                            Datum::Int4(i32::try_from(i % 100).unwrap()),
                            Datum::Text(format!("row {:012}", (i * 7919) % n)),
                        ]
                    })
                    .collect();
                rows::insert_many(&txn, &def, batch).await.unwrap();
            }
            txn.commit().await.unwrap();
            next = end;
        }
        eprintln!("loaded {n} rows in {:.1?}", started.elapsed());
        let txn = storage
            .begin(TxnOptions {
                read_only: true,
                ..TxnOptions::default()
            })
            .await
            .unwrap();

        let streaming = [
            "(plan (project (filter (get t) (< t.a 50)) t.id t.b))",
            "(plan (aggregate (get t) (keys t.k) (aggs (agg count) (agg sum t.id) (agg min t.b) (agg max t.a))))",
        ];
        // A first pass fills storage's block cache, which is bounded but not part of any query.
        run(&txn, streaming[0], DEFAULT_QUERY_BUDGET).await.unwrap();
        for text in streaming {
            let (rows, reserved, growth, seconds) =
                run(&txn, text, DEFAULT_QUERY_BUDGET).await.unwrap();
            eprintln!(
                "{text}: {rows} rows in {seconds:.1}s, reserved {reserved} bytes, resident growth {growth:?}"
            );
            assert!(rows > 0);
            assert!(
                reserved < 1024 * 1024,
                "{text} reserved {reserved} bytes over {n} rows"
            );
            if let Some(growth) = growth {
                assert!(
                    growth < STREAMING_SLACK,
                    "{text}: resident memory grew by {growth} bytes over {n} rows"
                );
            }
        }

        let blocking = "(plan (sort (get t) (asc t.b)))";
        let before = reset_peak();
        let started = Instant::now();
        let err = run(&txn, blocking, DEFAULT_QUERY_BUDGET)
            .await
            .unwrap_err();
        let growth = before
            .zip(status_bytes("VmHWM:"))
            .map(|(b, p)| p.saturating_sub(b));
        eprintln!(
            "{blocking}: failed after {:.1}s with \"{err}\", resident growth {growth:?}",
            started.elapsed().as_secs_f64()
        );
        assert!(
            matches!(err, ExecError::MemoryExceeded { operator: "Sort", .. }),
            "{err}"
        );
        if let Some(growth) = growth {
            // The sort stops at its budget; batches in flight and allocator slack come on top.
            let limit = 2 * u64::try_from(DEFAULT_QUERY_BUDGET).unwrap();
            assert!(growth < limit, "a failed sort grew memory by {growth} bytes");
        }
        txn.abort();
    });
}
