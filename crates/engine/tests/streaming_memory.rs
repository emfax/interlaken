//! Streaming results through a session in flat memory (ADR-0031, backlog
//! `engine-streaming-results`): the session-level counterpart of the
//! executor's 10-million-row criterion (`crates/exec/tests/ten_million.rs`).
//!
//! A counting global allocator (`interlaken-test-alloc`) tracks the heap
//! this process holds. Streaming a query's rows through
//! [`Session::query_stream`] must hold the heap roughly constant however
//! many rows it returns, while collecting them ([`Session::query`]) holds
//! them all. The normal suite streams [`SMALL_ROWS`] rows; the large
//! variant, run on demand, streams `STREAMING_ROWS` (default 3,000,000):
//!
//! ```text
//! cargo test --release -p interlaken-engine --test streaming_memory -- --ignored --nocapture
//! ```
//!
//! Everything in one test runs alone, since the counters are process-wide.

mod support;

use std::time::Instant;

use interlaken_engine::{Engine, Session};
use interlaken_storage::encoding::Datum;
use interlaken_storage::{Storage, TxnOptions, catalog, rows};
use interlaken_test_alloc::{Counting, held, peak, reset_peak};

use support::session::{admin, create, session, table};

#[global_allocator]
/// The process's allocator, counting the heap held.
static ALLOCATOR: Counting = Counting;

/// Rows in the normal suite's table.
const SMALL_ROWS: i64 = 300_000;

/// Rows per loading transaction.
const LOAD_TXN_ROWS: i64 = 100_000;

/// The most the heap may grow while a query streams, whatever its size.
const STREAMING_SLACK: usize = 16 * 1024 * 1024;

/// The streaming queries: half the rows' `id` and `b`, and an aggregation
/// into a thousand groups.
const QUERIES: [&str; 2] = [
    "(plan (project (filter (get t) (< t.a 50)) t.id t.b))",
    "(plan (aggregate (get t) (keys t.k) (aggs (agg count) (agg sum t.id) (agg min t.b) (agg max t.a))))",
];

/// Create `t(id int8 primary key, k int8, a int4, b text)` holding `n` rows
/// through `root`, loaded directly into storage.
///
/// Row `i` is `(i, i % 1000, i % 100, "row ...")` with a 16-character `b`.
async fn load(engine: &Engine, root: &mut Session, n: i64) {
    root.create_table(&table(
        "t",
        &[
            ("id", "(not-null int8)"),
            ("k", "int8"),
            ("a", "int4"),
            ("b", "text"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    let mut next = 0;
    while next < n {
        let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
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
}

/// What streaming one query measured.
struct Streamed {
    /// Rows returned.
    rows: usize,
    /// The bytes of every batch returned, as if collected.
    bytes: usize,
    /// The peak heap growth while streaming.
    growth: usize,
    /// How long it took.
    seconds: f64,
}

/// Stream `text` through `root`, dropping each batch once counted.
async fn stream(root: &mut Session, text: &str) -> Streamed {
    let before = reset_peak();
    let started = Instant::now();
    let mut stream = root.query_stream(text, Vec::new()).await.unwrap();
    let (mut rows, mut bytes) = (0, 0);
    while let Some(batch) = stream.next().await {
        let batch = batch.unwrap();
        rows += batch.num_rows();
        bytes += batch.get_array_memory_size();
    }
    drop(stream);
    Streamed {
        rows,
        bytes,
        growth: peak().saturating_sub(before),
        seconds: started.elapsed().as_secs_f64(),
    }
}

/// Load `n` rows, then assert every query streams in flat memory; with
/// `contrast`, also assert collecting the first query holds its rows.
fn streams_in_flat_memory(n: i64, contrast: bool) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    runtime.block_on(async {
        let engine = create(dir.path()).await;
        let mut root = session(&engine, admin());
        let started = Instant::now();
        load(&engine, &mut root, n).await;
        eprintln!("loaded {n} rows in {:.1?}", started.elapsed());
        // A first pass fills storage's block cache, which is bounded but not part of any query.
        stream(&mut root, QUERIES[0]).await;
        for text in QUERIES {
            let s = stream(&mut root, text).await;
            eprintln!(
                "{text}: streamed {} rows ({} bytes of batches) in {:.1}s, peak heap growth {} bytes",
                s.rows, s.bytes, s.seconds, s.growth
            );
            assert!(s.rows > 0);
            assert!(
                s.growth < STREAMING_SLACK,
                "{text}: the heap grew by {} bytes streaming {} rows",
                s.growth,
                s.rows
            );
        }
        let streamed = stream(&mut root, QUERIES[0]).await;
        assert_eq!(streamed.rows, usize::try_from(n / 2).unwrap());
        if contrast {
            // Collecting holds every batch: the growth streaming avoids.
            let before = reset_peak();
            let result = root.query(QUERIES[0], Vec::new()).await.unwrap();
            let growth = held().saturating_sub(before);
            eprintln!(
                "collected {} rows: heap held {growth} bytes more (streaming peaked at {})",
                result.row_count(),
                streamed.growth
            );
            assert!(growth >= streamed.bytes / 2, "collecting held only {growth} bytes");
            assert!(
                streamed.growth * 4 < growth,
                "streaming peaked at {} bytes, collecting held {growth}",
                streamed.growth
            );
        }
    });
}

#[test]
/// Assert streaming a few hundred thousand rows holds the heap flat, unlike collecting them.
fn streaming_holds_memory_flat() {
    streams_in_flat_memory(SMALL_ROWS, true);
}

#[test]
#[ignore = "loads millions of rows; run on demand with --ignored"]
/// Assert streaming millions of rows (`STREAMING_ROWS`) holds the heap flat.
fn streaming_millions_of_rows_holds_memory_flat() {
    let n: i64 = std::env::var("STREAMING_ROWS")
        .ok()
        .and_then(|v| v.replace('_', "").parse().ok())
        .unwrap_or(3_000_000);
    streams_in_flat_memory(n, false);
}
