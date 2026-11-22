//! Executor benchmarks (milestone F3): throughput and peak memory of each
//! operator family over millions of rows, through the engine's public API.
//!
//! One database is built per run: `t` holds `BENCH_ROWS` rows (default one
//! million), `dim` a tenth of that, `small` 10,000 rows and `docs` (governed
//! by a row-level policy) as many rows as `t`. Each case runs a statement
//! `BENCH_RUNS` times and reports the fastest and the median wall time, the
//! throughput over the case's input rows at the median, the most query
//! memory reserved at once (ADR-0027 reservations, from the engine's node
//! pool) and the growth of the process's peak resident set during the
//! statement (Linux only; the peak is reset before each run through
//! `/proc/self/clear_refs`). A statement that fails prints its error, so a
//! blocking operator over its budget shows as `53200`.
//!
//! Run with `cargo bench -p interlaken-engine --bench executor`. Optional
//! arguments filter cases by substring (for example `agg` or `index_nl`),
//! or by exact name with a leading `=` (`=not_in`).
//! Environment variables:
//!
//! - `BENCH_ROWS`: rows in `t` and `docs` (default 1,000,000).
//! - `BENCH_RUNS`: runs per case (default 3).
//! - `BENCH_BUDGET_MB`: the per-query memory budget in MiB (default 1024).
//! - `BENCH_DIR`: directory for the temporary database (default: the
//!   system temp dir).

use std::time::{Duration, Instant};

use interlaken_engine::catalog::{
    AttributeSpec, ColumnSpec, EntitySpec, IndexKey, IndexSpec, TableSpec,
};
use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
use interlaken_engine::{Engine, EngineError, EngineOptions, Session};
use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_storage::encoding::Datum;
use interlaken_storage::{Durability, Storage, StorageOptions, TxnOptions, catalog, rows};

/// Rows per loading transaction.
const LOAD_TXN_ROWS: usize = 50_000;

/// Rows in `small`, the subquery side of `NOT IN`.
const SMALL_ROWS: i64 = 10_000;

/// Outer rows probing an index in the index nested-loop cases.
const PROBE_ROWS: i64 = 10_000;

/// Outer rows of the `NOT IN` case.
const NOT_IN_OUTER_ROWS: i64 = 1_000;

/// Empty `t2`.
const CLEAR_T2: &str = "(plan (delete t2 (get t2)))";

/// Copy every row of `t` into `t2`.
const FILL_T2: &str = "(plan (insert t2 (project (get t :as x) x.id x.k x.g x.r x.a x.f x.s x.b) (columns id k g r a f s b)))";

/// The database's administrator.
fn admin() -> EntityUid {
    EntityUid::str("Admin", "root")
}

/// Return the type parsed from its text form.
fn ty(text: &str) -> interlaken_ir::Type {
    interlaken_ir::text::parse_type(text).unwrap()
}

/// Return a table spec from `(name, type)` columns, keyed by `id`.
fn table(name: &str, columns: &[(&str, &str)]) -> TableSpec {
    TableSpec {
        name: name.into(),
        columns: columns
            .iter()
            .map(|(n, t)| ColumnSpec::new(*n, ty(t)))
            .collect(),
        primary_key: vec!["id".into()],
    }
}

/// Return a pseudo-random number derived from `i` (splitmix64).
fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Return the value of an environment variable parsed, or `default`.
fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.replace('_', "").parse().ok())
        .unwrap_or(default)
}

/// Return the largest prime-ish multiplier coprime with `n`, for a permutation of `0..n`.
fn coprime(n: i64) -> i64 {
    /// Return the greatest common divisor.
    fn gcd(a: i64, b: i64) -> i64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    (7919..).find(|p| gcd(*p, n) == 1).unwrap()
}

/// Return one row of `t` for key `i` of `n`.
///
/// Columns: `k` (1,000 groups), `g` (`n / 10` groups, a key of `dim`),
/// `r` (a permutation of the keys, indexed), `a` (100 values), `f`, `s`, `b`.
fn t_row(i: i64, n: i64, p: i64) -> Vec<Datum> {
    let h = mix(u64::try_from(i).unwrap());
    let dims = (n / 10).max(1);
    #[allow(clippy::cast_precision_loss)]
    let f = (h % 1_000_000) as f64 / 7.0;
    vec![
        Datum::Int8(i),
        Datum::Int8(i % 1000),
        Datum::Int8((i * 7) % dims),
        Datum::Int8(i64::try_from((i128::from(i) * i128::from(p)) % i128::from(n)).unwrap()),
        Datum::Int4(i32::try_from(i % 100).unwrap()),
        Datum::Float8(f),
        Datum::Text(format!("s{:012}", h % 1_000_000_007)),
        Datum::Bool(i % 3 != 0),
    ]
}

/// Insert `count` rows produced by `row` into the stored table `name`, in several transactions.
async fn load(engine: &Engine, name: &str, count: i64, row: impl Fn(i64) -> Vec<Datum>) {
    let mut next = 0;
    while next < count {
        let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
        let def = catalog::table(&txn, name).await.unwrap().unwrap();
        let end = (next + i64::try_from(LOAD_TXN_ROWS).unwrap()).min(count);
        let batch: Vec<Vec<Datum>> = (next..end).map(&row).collect();
        for chunk in batch.chunks(1024) {
            rows::insert_many(&txn, &def, chunk.to_vec()).await.unwrap();
        }
        txn.commit().await.unwrap();
        next = end;
    }
}

/// Create the schema and policies and load every table.
async fn build(engine: &Engine, n: i64) {
    let mut admin = engine.session(SecurityContext::new(admin()));
    let t_columns = [
        ("id", "(option int8)"),
        ("k", "(option int8)"),
        ("g", "(option int8)"),
        ("r", "(option int8)"),
        ("a", "(option int4)"),
        ("f", "(option float8)"),
        ("s", "(option text)"),
        ("b", "(option bool)"),
    ];
    admin.create_table(&table("t", &t_columns)).await.unwrap();
    admin.create_table(&table("t2", &t_columns)).await.unwrap();
    admin
        .create_index(&IndexSpec {
            name: "t_r".into(),
            table: "t".into(),
            columns: vec![IndexKey::asc("r")],
            unique: false,
            nones_distinct: true,
        })
        .await
        .unwrap();
    admin
        .create_table(&table(
            "dim",
            &[
                ("id", "(option int8)"),
                ("name", "(option text)"),
                ("region", "(option int4)"),
            ],
        ))
        .await
        .unwrap();
    admin
        .create_table(&table(
            "small",
            &[("id", "(option int8)"), ("v", "(option int8)")],
        ))
        .await
        .unwrap();
    admin
        .create_table(&table(
            "users",
            &[("id", "(option int8)"), ("name", "(option text)")],
        ))
        .await
        .unwrap();
    admin
        .create_table(&table(
            "docs",
            &[
                ("id", "(option int8)"),
                ("owner_id", "int8"),
                ("title", "(option text)"),
                ("secret", "(option int8)"),
            ],
        ))
        .await
        .unwrap();
    admin
        .declare_entity(&EntitySpec {
            name: "User".into(),
            table: "users".into(),
            key: "id".into(),
            attributes: vec![AttributeSpec {
                name: "name".into(),
                column: "name".into(),
                references: None,
            }],
        })
        .await
        .unwrap();
    admin
        .declare_entity(&EntitySpec {
            name: "Doc".into(),
            table: "docs".into(),
            key: "id".into(),
            attributes: vec![
                AttributeSpec {
                    name: "owner".into(),
                    column: "owner_id".into(),
                    references: Some("User".into()),
                },
                AttributeSpec {
                    name: "title".into(),
                    column: "title".into(),
                    references: None,
                },
            ],
        })
        .await
        .unwrap();
    let document = PolicyDocument {
        language: PolicyLanguage::Rls,
        body: "(policies
            (permit (read) Doc (== resource.owner principal))
            (permit (read) User true))"
            .into(),
    };
    let id = admin.submit_policies(&document, "").await.unwrap();
    admin.verify_policies(id).await.unwrap();
    admin.activate_policies(id, true).await.unwrap();

    let started = Instant::now();
    let p = coprime(n);
    load(engine, "t", n, |i| t_row(i, n, p)).await;
    load(engine, "dim", (n / 10).max(1), |i| {
        vec![
            Datum::Int8(i),
            Datum::Text(format!("dim {i}")),
            Datum::Int4(i32::try_from(i % 50).unwrap()),
        ]
    })
    .await;
    load(engine, "small", SMALL_ROWS, |i| {
        vec![Datum::Int8(i), Datum::Int8((i * 3) % (2 * SMALL_ROWS))]
    })
    .await;
    load(engine, "users", 100, |i| {
        vec![Datum::Int8(i), Datum::Text(format!("user {i}"))]
    })
    .await;
    load(engine, "docs", n, |i| {
        vec![
            Datum::Int8(i),
            Datum::Int8(i % 100),
            Datum::Text(format!("doc {i}")),
            Datum::Int8(i % 977),
        ]
    })
    .await;
    eprintln!(
        "loaded {n} rows per large table in {:.1?}",
        started.elapsed()
    );
}

/// Who runs a case.
#[derive(Clone, Copy)]
enum Who {
    /// The administrator, whom no policy restricts (only `docs` and `users` are governed).
    Admin,
    /// User 1, who may read 1% of `docs`.
    User,
}

/// One benchmark case.
struct Case {
    /// The name filters match.
    name: &'static str,
    /// The statements, run in order in one measured run; all but the last are
    /// untimed setup, after which old versions are garbage collected.
    statements: Vec<String>,
    /// The principal.
    who: Who,
    /// The input rows the throughput is computed over.
    rows: i64,
}

/// Return every case for `n` rows.
fn cases(n: i64) -> Vec<Case> {
    let case = |name, text: &str, who, rows| Case {
        name,
        statements: vec![text.to_owned()],
        who,
        rows,
    };
    let dims = (n / 10).max(1);
    vec![
        case(
            "scan_count",
            "(plan (aggregate (get t) (keys) (aggs (agg count))))",
            Who::Admin,
            n,
        ),
        case(
            "scan_pushdown",
            "(plan (aggregate (filter (get t) (< t.a 10)) (keys) (aggs (agg count))))",
            Who::Admin,
            n,
        ),
        case(
            "scan_filter",
            "(plan (aggregate (filter (get t) (= (% t.id 10) 0)) (keys) (aggs (agg count))))",
            Who::Admin,
            n,
        ),
        case(
            "hash_join",
            "(plan (aggregate (join inner (get t) (get dim) (= t.g dim.id)) (keys) (aggs (agg count))))",
            Who::Admin,
            n + dims,
        ),
        case(
            "agg_scalar",
            "(plan (aggregate (get t) (keys) (aggs (agg sum t.id) (agg max t.s) (agg min t.f) (agg bool_or t.b))))",
            Who::Admin,
            n,
        ),
        case(
            "agg_few_groups",
            "(plan (aggregate (get t) (keys t.k) (aggs (agg sum t.id) (agg min t.s) (agg max t.f) (agg bool_and t.b) (agg avg t.a))))",
            Who::Admin,
            n,
        ),
        case(
            "agg_many_groups",
            "(plan (aggregate (aggregate (get t) (keys t.g) (aggs (agg sum t.id) (agg max t.a))) (keys) (aggs (agg count))))",
            Who::Admin,
            n,
        ),
        case(
            "agg_float_sum",
            "(plan (aggregate (get t) (keys t.k) (aggs (agg sum t.f) (agg avg t.f))))",
            Who::Admin,
            n,
        ),
        case(
            "sort",
            "(plan (aggregate (sort (get t) (asc t.s)) (keys) (aggs (agg count))))",
            Who::Admin,
            n,
        ),
        case(
            "topn",
            "(plan (limit (sort (get t) (desc t.f)) :count 10))",
            Who::Admin,
            n,
        ),
        case(
            "index_nl_pk",
            &format!(
                "(plan (aggregate (join inner (filter (get t :as x) (< x.id {PROBE_ROWS})) (get t :as y) (= x.r y.id)) (keys) (aggs (agg count))))"
            ),
            Who::Admin,
            PROBE_ROWS,
        ),
        case(
            "index_nl_pk_wide",
            &format!(
                "(plan (aggregate (join inner (filter (get t :as x) (< x.id {})) (get t :as y) (= x.r y.id)) (keys) (aggs (agg count))))",
                10 * PROBE_ROWS
            ),
            Who::Admin,
            10 * PROBE_ROWS,
        ),
        case(
            "index_nl_pk_all",
            "(plan (aggregate (join inner (get t :as x) (get t :as y) (= x.r y.id)) (keys) (aggs (agg count))))",
            Who::Admin,
            n,
        ),
        case(
            "index_nl_secondary",
            &format!(
                "(plan (aggregate (join inner (filter (get t :as x) (< x.id {PROBE_ROWS})) (get t :as y) (= x.id y.r)) (keys) (aggs (agg count))))"
            ),
            Who::Admin,
            PROBE_ROWS,
        ),
        case(
            "not_in",
            &format!(
                "(plan (aggregate (filter (get t :as x) (and (< x.id {NOT_IN_OUTER_ROWS}) (not (in x.g (project (get small) small.v))))) (keys) (aggs (agg count))))"
            ),
            Who::Admin,
            NOT_IN_OUTER_ROWS,
        ),
        case(
            "not_in_large",
            "(plan (aggregate (filter (get t :as x) (not (in x.g (project (get small) small.v)))) (keys) (aggs (agg count))))",
            Who::Admin,
            n,
        ),
        case(
            "secured_scan",
            "(plan (aggregate (get docs :as d) (keys) (aggs (agg count) (agg sum d.secret))))",
            Who::User,
            n,
        ),
        case(
            "secured_join",
            "(plan (aggregate (join inner (get docs :as d) (get users :as u) (= d.owner_id u.id)) (keys u.name) (aggs (agg count))))",
            Who::User,
            n,
        ),
        Case {
            name: "dml_insert",
            statements: vec![CLEAR_T2.into(), FILL_T2.into()],
            who: Who::Admin,
            rows: n,
        },
        Case {
            name: "dml_update",
            statements: vec![
                CLEAR_T2.into(),
                FILL_T2.into(),
                "(plan (update t2 (get t2) (set a (+ t2.a 1))))".into(),
            ],
            who: Who::Admin,
            rows: n,
        },
        Case {
            name: "dml_delete",
            statements: vec![CLEAR_T2.into(), FILL_T2.into(), CLEAR_T2.into()],
            who: Who::Admin,
            rows: n,
        },
    ]
}

/// Return a field of `/proc/self/status` in KiB, if available.
fn proc_status_kib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with(field))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Reset the process's peak resident set to its current size; return whether that worked.
fn reset_peak_rss() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

/// One measured run.
struct Run {
    /// Wall time of the timed statement.
    elapsed: Duration,
    /// The most query memory reserved at once, in bytes.
    reserved: usize,
    /// Growth of the peak resident set during the statement, in KiB.
    rss_kib: Option<u64>,
}

/// Run `case`'s statements once, timing the last.
async fn run_once(engine: &Engine, case: &Case) -> Result<Run, EngineError> {
    let context = match case.who {
        Who::Admin => SecurityContext::new(admin()),
        Who::User => SecurityContext::new(EntityUid::long("User", 1)),
    };
    let mut session: Session = engine.session(context);
    let (timed, setup) = case.statements.split_last().unwrap();
    for text in setup {
        session.query(text, Vec::new()).await?;
    }
    // Collect the versions earlier runs left, so every run reads the same data.
    engine.storage().run_gc().await?;
    let pool = engine.memory_pool().unwrap();
    pool.reset_peak();
    let before = proc_status_kib("VmRSS:");
    let tracked = reset_peak_rss();
    let started = Instant::now();
    session.query(timed, Vec::new()).await?;
    let elapsed = started.elapsed();
    let rss_kib = match (tracked, before, proc_status_kib("VmHWM:")) {
        (true, Some(before), Some(peak)) => Some(peak.saturating_sub(before)),
        _ => None,
    };
    Ok(Run {
        elapsed,
        reserved: pool.peak(),
        rss_kib,
    })
}

/// Format a byte count in MiB.
#[allow(clippy::cast_precision_loss)]
fn mib(bytes: usize) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

/// Run each selected case and print its figures.
fn main() {
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let n: i64 = env("BENCH_ROWS", 1_000_000);
    let runs: usize = env("BENCH_RUNS", 3);
    let budget_mb: usize = env("BENCH_BUDGET_MB", 1024);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let dir = match std::env::var("BENCH_DIR") {
        Ok(base) => tempfile::tempdir_in(base).unwrap(),
        Err(_) => tempfile::tempdir().unwrap(),
    };
    let options = EngineOptions {
        storage: StorageOptions {
            durability: Durability::Buffered,
            gc_interval: None,
            // Bulk DML over every row of `t` writes far more than the default allows.
            max_write_set_bytes: 4 << 30,
            ..StorageOptions::default()
        },
        query_budget: budget_mb * 1024 * 1024,
        node_memory_limit: Some(usize::MAX / 2),
        ..EngineOptions::default()
    };
    let selected: Vec<Case> = cases(n)
        .into_iter()
        .filter(|c| {
            filters.is_empty()
                || filters.iter().any(|f| match f.strip_prefix('=') {
                    Some(exact) => c.name == exact,
                    None => c.name.contains(f.as_str()),
                })
        })
        .collect();
    if selected.is_empty() {
        eprintln!("no case matches {filters:?}");
        return;
    }
    let engine = runtime.block_on(async {
        let engine = Engine::create(dir.path(), options, &admin()).await.unwrap();
        build(&engine, n).await;
        engine
    });
    println!("rows={n} runs={runs} budget={budget_mb}MiB");
    println!(
        "{:<20} {:>10} {:>10} {:>14} {:>12} {:>12}",
        "case", "best", "median", "rows/s", "reserved", "rss_growth"
    );
    for case in &selected {
        let mut results = Vec::new();
        let mut failure = None;
        for _ in 0..runs {
            match runtime.block_on(run_once(&engine, case)) {
                Ok(run) => results.push(run),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = failure {
            println!("{:<20} failed: [{}] {e}", case.name, e.sqlstate());
            continue;
        }
        results.sort_by_key(|r| r.elapsed);
        let best = results[0].elapsed;
        let median = &results[results.len() / 2];
        #[allow(clippy::cast_precision_loss)]
        let throughput = case.rows as f64 / median.elapsed.as_secs_f64();
        let reserved = results.iter().map(|r| r.reserved).max().unwrap_or(0);
        let rss = results
            .iter()
            .filter_map(|r| r.rss_kib)
            .max()
            .map_or_else(|| "n/a".to_owned(), |k| format!("{:.1}", k as f64 / 1024.0));
        println!(
            "{:<20} {:>10.3?} {:>10.3?} {:>14.0} {:>12} {:>12}",
            case.name,
            best,
            median.elapsed,
            throughput,
            mib(reserved),
            rss
        );
    }
}
