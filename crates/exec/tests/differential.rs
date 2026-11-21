//! Differential tests: the executor against the reference interpreter (ADR-0022, milestone E1).
//!
//! Random rows are written into a real stored table, inside a transaction
//! that is always rolled back, and into the interpreter's database. Random
//! queries of scans, filters, projections and limits run through both,
//! optimized or not, with small storage batches so scans resume across
//! many calls and filters empty whole batches.
//!
//! Whenever the interpreter succeeds, the executor must return the same
//! rows in the same order (both read in primary key order). If the
//! interpreter fails, the executor must fail too, unless the query has a
//! limit (the executor may stop before the failing row) or a filter
//! (leakproof conjuncts run in storage first and may discard the row, as
//! Postgres may evaluate conjuncts in any order).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_exec::{ExecContext, PlanOptions, collect, datum_at, plan_with};
use interlaken_ir::TableId;
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Database, Datum, ExecOptions, execute as interpret};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan, print_plan};
use interlaken_ir::verify::{VerifyOptions, verify};
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum as Stored};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions, rows};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

/// The IR catalog: one table matching the stored one.
fn ir_catalog() -> InMemoryCatalog {
    parse_catalog(
        "(catalog (builtins)
           (table t :id 100
             (column id int8) (column a (option int4)) (column b (option text)) (column c (option float8))
             (column d (option bool)) (column e (option int2))
             (primary-key id)))",
    )
    .unwrap()
}

/// One generated row: a, b, c, d, e (the key is assigned).
type RowData = (
    Option<i32>,
    Option<String>,
    Option<f64>,
    Option<bool>,
    Option<i16>,
);

/// Generate a row with small, colliding values and NULLs.
fn row() -> impl Strategy<Value = RowData> {
    (
        proptest::option::of(-5i32..20),
        proptest::option::of("[ab%_\\\\]{0,3}"),
        proptest::option::of(prop_oneof![Just(0.0), Just(-1.5), Just(2.25), Just(1e300)]),
        proptest::option::of(any::<bool>()),
        proptest::option::of(prop_oneof![Just(0i16), Just(3), Just(i16::MAX)]),
    )
}

/// Generate an integer expression over columns `cols` (names of int8/int4/int2 columns);
/// `table` says whether the table's text and float columns are in scope too.
fn int_expr(cols: Vec<String>, table: bool) -> BoxedStrategy<String> {
    let leaf = prop_oneof![
        3 => proptest::sample::select(cols),
        1 => (-3i64..10).prop_map(|v| v.to_string()),
    ];
    leaf.prop_recursive(2, 6, 2, move |inner| {
        let mut options = vec![
            (
                proptest::sample::select(vec!["+", "-", "*", "/", "%"]),
                inner.clone(),
                inner.clone(),
            )
                .prop_map(|(op, l, r)| format!("({op} {l} {r})"))
                .boxed(),
            inner
                .clone()
                .prop_map(|e| format!("(call abs {e})"))
                .boxed(),
            (inner.clone(), inner.clone())
                .prop_map(|(x, y)| format!("(coalesce {x} {y})"))
                .boxed(),
            (inner.clone(), inner.clone())
                .prop_map(|(x, y)| format!("(case (when (> {x} 2) {x}) (else {y}))"))
                .boxed(),
        ];
        if table {
            options.push(Just("(call length b)".to_owned()).boxed());
            options.push(Just("(cast c int8)".to_owned()).boxed());
        }
        proptest::strategy::Union::new(options)
    })
    .boxed()
}

/// Generate a boolean expression.
fn bool_expr(ints: Vec<String>, has_text: bool) -> BoxedStrategy<String> {
    let i = int_expr(ints, has_text);
    let cmp = proptest::sample::select(vec!["=", "<>", "<", "<=", ">", ">="]);
    let mut atoms = vec![
        (cmp, i.clone(), i.clone())
            .prop_map(|(op, l, r)| format!("({op} {l} {r})"))
            .boxed(),
        i.clone().prop_map(|e| format!("(is-null {e})")).boxed(),
        Just("true".to_owned()).boxed(),
    ];
    if has_text {
        atoms.push(
            prop_oneof![Just("a%"), Just("%b"), Just("_"), Just("\\\\")]
                .prop_map(|p| format!("(like b {p:?})"))
                .boxed(),
        );
        atoms.push(Just("(is-not-null d)".to_owned()).boxed());
        atoms.push(Just("(= d true)".to_owned()).boxed());
    }
    proptest::strategy::Union::new(atoms)
        .prop_recursive(2, 6, 2, |inner| {
            prop_oneof![
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("(and {a} {b})")),
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("(or {a} {b})")),
                inner.prop_map(|a| format!("(not {a})")),
            ]
        })
        .boxed()
}

/// One step on top of the scan.
#[derive(Clone, Debug)]
enum Step {
    /// A filter.
    Filter,
    /// A projection of `n` integer columns (plus the text column while the table's are in scope).
    Project(usize),
    /// A limit and offset.
    Limit(i64, i64),
}

/// Generate a query: a scan of `t` with up to three steps on top.
fn query() -> impl Strategy<Value = String> {
    let step = prop_oneof![
        3 => Just(Step::Filter),
        2 => (1usize..4).prop_map(Step::Project),
        1 => (0i64..8, 0i64..4).prop_map(|(l, o)| Step::Limit(l, o)),
    ];
    proptest::collection::vec(step, 0..4).prop_flat_map(|steps| {
        // Build expression strategies for each step's scope, then render the query.
        let mut scope_ints: Vec<String> = vec!["id".into(), "a".into(), "e".into()];
        let mut has_text = true;
        let mut parts: Vec<BoxedStrategy<(Step, Vec<String>)>> = Vec::new();
        for step in steps {
            match step {
                Step::Filter => {
                    let s = step.clone();
                    parts.push(
                        bool_expr(scope_ints.clone(), has_text)
                            .prop_map(move |e| (s.clone(), vec![e]))
                            .boxed(),
                    );
                }
                Step::Project(n) => {
                    let s = step.clone();
                    let items: Vec<BoxedStrategy<String>> = (0..n)
                        .map(|_| int_expr(scope_ints.clone(), has_text))
                        .collect();
                    parts.push(items.prop_map(move |e| (s.clone(), e)).boxed());
                    scope_ints = (0..n).map(|i| format!("x{i}")).collect();
                    has_text = false;
                }
                Step::Limit(..) => parts.push(Just((step.clone(), Vec::new())).boxed()),
            }
        }
        parts.prop_map(|parts| {
            let mut rel = "(get t)".to_owned();
            for (step, exprs) in parts {
                rel = match step {
                    Step::Filter => format!("(filter {rel} {})", exprs[0]),
                    Step::Project(_) => {
                        let items: Vec<String> = exprs
                            .iter()
                            .enumerate()
                            .map(|(i, e)| format!("(as {e} x{i})"))
                            .collect();
                        format!("(project {rel} {})", items.join(" "))
                    }
                    Step::Limit(l, o) => format!("(limit {rel} :count {l} :offset {o})"),
                };
            }
            format!("(plan {rel})")
        })
    })
}

#[test]
/// Assert the executor returns what the reference interpreter returns.
fn executor_agrees_with_interpreter() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = runtime.block_on(async {
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
        let spec = TableSpec {
            name: "t".into(),
            columns: vec![
                ColumnDef::new("id", ColumnType::Int8),
                ColumnDef::new("a", ColumnType::Int4),
                ColumnDef::new("b", ColumnType::Text),
                ColumnDef::new("c", ColumnType::Float8),
                ColumnDef::new("d", ColumnType::Bool),
                ColumnDef::new("e", ColumnType::Int2),
            ],
            primary_key: vec!["id".into()],
        };
        catalog::create_table(&txn, spec).await.unwrap();
        txn.commit().await.unwrap();
        storage
    });
    let catalog = Arc::new(ir_catalog());
    let shared: Arc<dyn Catalog> = catalog.clone();
    let cases = 1500;
    let (compared, multi_batch) = (AtomicU32::new(0), AtomicU32::new(0));
    let strategy = (
        proptest::collection::vec(row(), 0..25),
        query(),
        1usize..6,
        any::<bool>(),
    );
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(&strategy, |(data, query, batch_rows, optimized)| {
        let plan = resolve(&parse_plan(&query).unwrap(), catalog.as_ref())
            .unwrap_or_else(|e| panic!("{e}: {query}"))
            .plan;
        let plan = if optimized {
            optimize(&plan, catalog.as_ref())
        } else {
            plan
        };
        verify(&plan, catalog.as_ref(), &VerifyOptions::default()).unwrap();

        // The interpreter's view of the data, in primary key order.
        let rows: Vec<Vec<Datum>> = data
            .iter()
            .enumerate()
            .map(|(i, (a, b, c, d, e))| {
                vec![
                    Datum::Int8(i as i64),
                    a.map_or(Datum::Null, Datum::Int4),
                    b.clone().map_or(Datum::Null, Datum::Text),
                    c.map_or(Datum::Null, Datum::Float8),
                    d.map_or(Datum::Null, Datum::Bool),
                    e.map_or(Datum::Null, Datum::Int2),
                ]
            })
            .collect();
        let mut db = Database::new();
        db.set_rows(TableId::new(100), rows.clone());
        let expected = interpret(
            &plan,
            catalog.as_ref(),
            &mut db,
            &[],
            ExecOptions::default(),
        );

        let physical = plan_with(&plan, catalog.as_ref(), PlanOptions { batch_rows }).unwrap();
        let actual = runtime.block_on(async {
            let txn = storage.begin(TxnOptions::default()).await.unwrap();
            let table = catalog::table(&txn, "t").await.unwrap().unwrap();
            for row in &rows {
                let stored = row
                    .iter()
                    .map(|d| match d {
                        Datum::Null => Stored::Null,
                        Datum::Int8(v) => Stored::Int8(*v),
                        Datum::Int4(v) => Stored::Int4(*v),
                        Datum::Int2(v) => Stored::Int2(*v),
                        Datum::Float8(v) => Stored::Float8(*v),
                        Datum::Bool(v) => Stored::Bool(*v),
                        Datum::Text(v) => Stored::Text(v.clone()),
                        other => panic!("unexpected {other:?}"),
                    })
                    .collect();
                rows::insert(&txn, &table, stored).await.unwrap();
            }
            txn.next_statement();
            let ctx = ExecContext::new(
                txn.clone(),
                shared.clone(),
                Vec::new(),
                ExecOptions::default(),
            );
            let result = collect(&physical, &ctx).await;
            txn.abort();
            result
        });
        let describe = || format!("{}\n{data:?}\nbatch rows {batch_rows}", print_plan(&plan));
        match (expected, actual) {
            (Ok(expected), Ok(batches)) => {
                let mut got = Vec::new();
                for batch in &batches {
                    prop_assert!(
                        batch.num_rows() > 0 || batches.len() == 1,
                        "empty batch emitted\n{}",
                        describe()
                    );
                    for r in 0..batch.num_rows() {
                        got.push(
                            batch
                                .columns()
                                .iter()
                                .map(|c| datum_at(c.as_ref(), r).unwrap())
                                .collect::<Vec<_>>(),
                        );
                    }
                }
                prop_assert_eq!(&got, &expected, "{}", describe());
                compared.fetch_add(1, Ordering::Relaxed);
                if batches.len() > 1 {
                    multi_batch.fetch_add(1, Ordering::Relaxed);
                }
            }
            (Ok(expected), Err(e)) => {
                return Err(TestCaseError::fail(format!(
                    "executor failed ({e}) where the interpreter returned {expected:?}\n{}",
                    describe()
                )));
            }
            (Err(e), Ok(got)) if !query.contains("(limit") && !query.contains("(filter") => {
                return Err(TestCaseError::fail(format!(
                    "executor returned {} batches where the interpreter failed ({e})\n{}",
                    got.len(),
                    describe()
                )));
            }
            _ => {}
        }
        Ok(())
    })
    .unwrap();
    let (compared, multi_batch) = (
        compared.load(Ordering::Relaxed),
        multi_batch.load(Ordering::Relaxed),
    );
    eprintln!("compared {compared} of {cases}; {multi_batch} spanned several batches");
    assert!(
        compared * 2 >= cases,
        "only {compared} of {cases} cases compared"
    );
    assert!(
        multi_batch * 4 >= compared,
        "too few multi-batch results: {multi_batch}"
    );
}
