//! Differential tests of the F3 operator paths against the reference interpreter.
//!
//! Three families of random plans run on real storage and in the
//! interpreter, over tables of up to a few hundred rows (many per group):
//!
//! - **Incremental aggregates:** every accumulating aggregate (`count`,
//!   `sum`, `avg`, `min`, `max`, `bool_and`, `bool_or`) over few, large
//!   groups, with `FILTER` and `DISTINCT`, NULLs, integer sums that overflow
//!   and floats whose sums depend on order (`1e16 + 1 - 1e16`), negative
//!   zero, infinities and NaN. Aggregates read a scan directly, so rows reach
//!   them in primary key order in both, and float results must be identical.
//! - **Index lookup joins:** joins of every kind whose right input is a table
//!   (filtered or not) keyed by its primary key, a primary key prefix, or a
//!   unique or non-unique secondary index, with NULL keys, residuals and left
//!   inputs smaller and larger than the right, so lookups and the switch to a
//!   hash join both run.
//! - **Quantified comparisons:** `IN`, `NOT IN`, `op ANY` and `op ALL` over
//!   subqueries with NULLs, correlated or not, in filters and projections,
//!   optimized (decorrelated into joins) or not (nested loops).
//!
//! Rows compare as bags. The executor must never fail where the interpreter
//! succeeds; where the interpreter fails, it may succeed (it evaluates less).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_exec::{ExecContext, Op, Physical, PlanOptions, collect, datum_at, plan_with};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Database, Datum, ExecOptions, RowKey, execute as interpret};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan, print_plan};
use interlaken_ir::{Rel, TableId};
use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableSpec};
use interlaken_storage::encoding::{ColumnType, SortOrder};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions, rows};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

/// The IR catalog matching the stored tables.
const CATALOG: &str = "(catalog (builtins)
  (table a :id 100
    (column id int8) (column k (option int8)) (column g (option int4)) (column n (option int8))
    (column f (option float8)) (column s (option text)) (column b (option bool))
    (primary-key id))
  (table b :id 101
    (column id int8) (column x (option int8)) (column u (option int8)) (column label (option text))
    (primary-key id)
    (index b_x x)
    (index b_u u :unique true))
  (table c :id 102
    (column p int8) (column q int8) (column v (option int4))
    (primary-key p q)))";

/// A deterministic generator (xorshift64*), seeded per case.
struct Gen {
    /// The random state.
    state: u64,
}

impl Gen {
    /// Return the next random number.
    fn next(&mut self) -> u64 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Return a number below `n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Return a signed number below `n`.
    fn int(&mut self, n: u64) -> i64 {
        i64::try_from(self.below(n)).unwrap()
    }

    /// Return true with probability `percent`/100.
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// Return one of `items`.
    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[usize::try_from(self.below(items.len() as u64)).unwrap()].clone()
    }

    /// Return the value `make` generates, or NULL with probability `percent`/100.
    fn or_null(&mut self, percent: u64, make: impl FnOnce(&mut Self) -> Datum) -> Datum {
        if self.chance(percent) {
            Datum::Null
        } else {
            make(self)
        }
    }

    /// Generate the rows of `a`, `b` and `c`.
    fn data(&mut self) -> [Vec<Vec<Datum>>; 3] {
        let floats = [
            0.1,
            0.25,
            1e16,
            -1e16,
            1.0,
            -0.0,
            0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        let huge = self.chance(15);
        let a = (0..self.int(300))
            .map(|i| {
                let n = if huge {
                    i64::MAX / 3
                } else {
                    self.int(1000) - 500
                };
                vec![
                    Datum::Int8(i),
                    self.or_null(10, |g| Datum::Int8(g.int(60))),
                    self.or_null(5, |g| Datum::Int4(i32::try_from(g.int(3)).unwrap())),
                    self.or_null(10, |_| Datum::Int8(n)),
                    self.or_null(10, |g| Datum::Float8(g.pick(&floats))),
                    self.or_null(10, |g| Datum::Text(format!("s{}", g.below(30)))),
                    self.or_null(10, |g| Datum::Bool(g.chance(70))),
                ]
            })
            .collect();
        let b = (0..self.int(300))
            .map(|i| {
                vec![
                    Datum::Int8(i),
                    self.or_null(10, |g| Datum::Int8(g.int(60))),
                    // Unique when present: ids shifted, with gaps.
                    self.or_null(30, |_| Datum::Int8(2 * i + 1)),
                    self.or_null(10, |g| Datum::Text(format!("l{}", g.below(5)))),
                ]
            })
            .collect();
        let mut c = Vec::new();
        for p in 0..self.int(60) {
            for q in 0..self.int(6) {
                c.push(vec![
                    Datum::Int8(p),
                    Datum::Int8(q),
                    self.or_null(10, |g| Datum::Int4(i32::try_from(g.int(9)).unwrap())),
                ]);
            }
        }
        [a, b, c]
    }

    /// Return an optional ` :filter ...` clause over `a`.
    fn agg_filter(&mut self) -> &'static str {
        if self.chance(25) {
            self.pick(&[
                " :filter a.b",
                " :filter (> a.n 0)",
                " :filter (is-null a.s)",
            ])
        } else {
            ""
        }
    }

    /// Return an aggregation of `a` over few, large groups.
    fn aggregate(&mut self) -> String {
        let keys = self.pick(&["", "a.g", "a.b", "a.g a.b"]);
        let mut aggs = String::new();
        for i in 0..1 + self.below(4) {
            let distinct = if self.chance(15) {
                " :distinct true"
            } else {
                ""
            };
            let call = match self.below(13) {
                0 => "(agg count)".to_owned(),
                1 => format!("(agg count a.s{distinct})"),
                2 => format!("(agg sum a.n{distinct})"),
                3 => format!("(agg sum a.g{distinct})"),
                4 => format!("(agg sum a.f{distinct})"),
                5 => "(agg avg a.n)".to_owned(),
                6 => "(agg avg a.f)".to_owned(),
                7 => format!("(agg {} a.s)", self.pick(&["min", "max"])),
                8 => format!("(agg {} a.f)", self.pick(&["min", "max"])),
                9 => format!("(agg {} a.n)", self.pick(&["min", "max"])),
                10 => format!("(agg {} a.b)", self.pick(&["min", "max"])),
                11 => format!("(agg {} a.b)", self.pick(&["bool_and", "bool_or"])),
                _ => "(agg sum (+ a.k a.g))".to_owned(),
            };
            let filter = self.agg_filter();
            write!(aggs, " (as {}{filter}) x{i})", &call[..call.len() - 1]).unwrap();
        }
        let input = if self.chance(30) {
            format!("(filter (get a) (< a.id {}))", self.below(300))
        } else {
            "(get a)".to_owned()
        };
        format!("(plan (aggregate {input} (keys {keys}) (aggs{aggs})))")
    }

    /// Return a join whose right input is a keyed table.
    fn lookup_join(&mut self) -> String {
        let kind = self.pick(&[
            "inner", "inner", "left", "left", "semi", "anti", "right", "full",
        ]);
        // Mostly few left rows, which look up; sometimes many, which hash.
        let left = match self.below(4) {
            0 => "(get a :as l)".to_owned(),
            1 => format!("(filter (get a :as l) (< l.id {}))", self.below(300)),
            _ => format!("(filter (get a :as l) (< l.id {}))", self.below(60)),
        };
        let (right, key, cols) = match self.below(8) {
            0 | 1 => ("(get b :as r)", "(= l.k r.id)", "r.id r.label"),
            2 => ("(get b :as r)", "(= r.x l.k)", "r.id r.x"),
            3 => ("(get b :as r)", "(= l.n r.u)", "r.u r.label"),
            // A key beyond the one looked up must match too.
            4 => (
                "(get b :as r)",
                "(and (= l.k r.id) (= l.n r.x))",
                "r.id r.x",
            ),
            5 => ("(get c :as r)", "(= l.k r.p)", "r.p r.q r.v"),
            6 => (
                "(get c :as r)",
                "(and (= (cast l.g int8) r.q) (= l.k r.p))",
                "r.p r.q r.v",
            ),
            _ => ("(get c :as r)", "(and (= l.k r.p) (= l.g r.v))", "r.p r.q"),
        };
        let right = if self.chance(40) {
            let condition = match right {
                "(get c :as r)" => self.pick(&["(> r.q 1)", "(is-not-null r.v)", "(< r.v 5)"]),
                _ => self.pick(&[
                    "(< r.id 150)",
                    "(is-not-null r.label)",
                    "(<> r.label \"l1\")",
                ]),
            };
            format!("(filter {right} {condition})")
        } else {
            right.to_owned()
        };
        let on = match self.below(4) {
            0 if right.contains(":as r") && !right.contains("get c") => {
                format!("(and {key} (> (+ r.id l.g) 3))")
            }
            1 => format!("(and {key} (or l.b (is-null l.s)))"),
            _ => key.to_owned(),
        };
        let columns = if matches!(kind, "semi" | "anti") {
            "l.id l.k".to_owned()
        } else {
            format!("l.id l.k {cols}")
        };
        format!("(plan (project (join {kind} {left} {right} {on}) {columns}))")
    }

    /// Return a query with a quantified comparison against `b`.
    fn quantified(&mut self) -> String {
        let outer = self.pick(&["o.k", "o.n", "(cast o.g int8)"]);
        let column = self.pick(&["i.x", "i.u", "i.id"]);
        let filter = match self.below(4) {
            0 => " (= i.x o.k)".to_owned(),
            1 => format!(" (< i.id {})", self.below(200)),
            2 => " (is-not-null i.label)".to_owned(),
            _ => String::new(),
        };
        let sub = if filter.is_empty() {
            format!("(project (get b :as i) {column})")
        } else {
            format!("(project (filter (get b :as i){filter}) {column})")
        };
        let comparison = match self.below(6) {
            0 => format!("(in {outer} {sub})"),
            1 | 2 => format!("(not (in {outer} {sub}))"),
            3 => format!(
                "(any {} {outer} {sub})",
                self.pick(&["<", "<=", ">", ">=", "=", "<>"])
            ),
            _ => format!(
                "(all {} {outer} {sub})",
                self.pick(&["<", "<=", ">", ">=", "=", "<>"])
            ),
        };
        let source = if self.chance(50) {
            "(get a :as o)".to_owned()
        } else {
            format!("(filter (get a :as o) (< o.id {}))", self.below(300))
        };
        if self.chance(70) {
            format!("(plan (project (filter {source} {comparison}) o.id))")
        } else {
            format!("(plan (project {source} o.id (as {comparison} q)))")
        }
    }
}

/// Count rows by value.
fn bag(rows: &[Vec<Datum>]) -> HashMap<RowKey, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(RowKey(row.clone())).or_insert(0) += 1;
    }
    counts
}

/// Collect the index lookups of a plan: (point lookups, prefix lookups).
fn lookups(node: &Physical, counts: &mut [u32; 3]) {
    if let Op::IndexLookup { point, key, .. } = &node.op {
        let i = match (point, key) {
            (true, _) => 0,
            (false, interlaken_exec::LookupKey::Primary) => 1,
            (false, interlaken_exec::LookupKey::Index(_)) => 2,
        };
        counts[i] += 1;
    }
    for input in node.op.inputs() {
        lookups(input, counts);
    }
}

/// Return whether a plan still has a correlated apply.
fn has_apply(rel: &Rel) -> bool {
    matches!(rel, Rel::Apply { .. }) || rel.inputs().into_iter().any(has_apply)
}

/// A stored table: its name, (column, type) pairs and primary key columns.
type StoredTable = (
    &'static str,
    &'static [(&'static str, ColumnType)],
    &'static [&'static str],
);

/// Storage with the tables of [`CATALOG`], created empty.
fn storage(runtime: &tokio::runtime::Runtime, dir: &std::path::Path) -> FjallStorage {
    runtime.block_on(async {
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
        let tables: [StoredTable; 3] = [
            (
                "a",
                &[
                    ("id", ColumnType::Int8),
                    ("k", ColumnType::Int8),
                    ("g", ColumnType::Int4),
                    ("n", ColumnType::Int8),
                    ("f", ColumnType::Float8),
                    ("s", ColumnType::Text),
                    ("b", ColumnType::Bool),
                ],
                &["id"],
            ),
            (
                "b",
                &[
                    ("id", ColumnType::Int8),
                    ("x", ColumnType::Int8),
                    ("u", ColumnType::Int8),
                    ("label", ColumnType::Text),
                ],
                &["id"],
            ),
            (
                "c",
                &[
                    ("p", ColumnType::Int8),
                    ("q", ColumnType::Int8),
                    ("v", ColumnType::Int4),
                ],
                &["p", "q"],
            ),
        ];
        for (name, columns, key) in tables {
            let spec = TableSpec {
                name: name.into(),
                columns: columns
                    .iter()
                    .map(|(c, ty)| {
                        let def = ColumnDef::new(*c, *ty);
                        if key.contains(c) { def.not_null() } else { def }
                    })
                    .collect(),
                primary_key: key.iter().map(|k| (*k).to_owned()).collect(),
            };
            catalog::create_table(&txn, spec).await.unwrap();
        }
        for (name, column, unique) in [("b_x", "x", false), ("b_u", "u", true)] {
            catalog::create_index(
                &txn,
                IndexSpec {
                    name: name.into(),
                    table: "b".into(),
                    columns: vec![(column.into(), SortOrder::ASC)],
                    unique,
                    nulls_distinct: true,
                },
            )
            .await
            .unwrap();
        }
        txn.commit().await.unwrap();
        storage
    })
}

#[test]
/// Assert aggregates, index lookup joins and quantified comparisons agree with the interpreter.
#[allow(clippy::too_many_lines)]
fn f3_operators_agree_with_interpreter() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = storage(&runtime, dir.path());
    let catalog: Arc<InMemoryCatalog> = Arc::new(parse_catalog(CATALOG).unwrap());
    let shared: Arc<dyn Catalog> = catalog.clone();
    let cases = 800;
    let counters: [AtomicU32; 9] = Default::default();
    let [
        compared,
        interp_failed,
        both_failed,
        point,
        prefix,
        secondary,
        decorrelated,
        aggregates,
        quantified,
    ] = &counters;
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(
        &(any::<u64>(), any::<bool>(), 0usize..4),
        |(seed, optimized, batch)| {
            let mut g = Gen { state: seed | 1 };
            let family = g.below(3);
            let text = match family {
                0 => g.aggregate(),
                1 => g.lookup_join(),
                _ => g.quantified(),
            };
            let plan = resolve(&parse_plan(&text).unwrap(), catalog.as_ref())
                .unwrap_or_else(|e| panic!("generated plan does not resolve: {e}\n{text}"))
                .plan;
            let plan = if optimized {
                optimize(&plan, catalog.as_ref())
            } else {
                plan
            };
            match family {
                0 => aggregates.fetch_add(1, Ordering::Relaxed),
                1 => 0,
                _ => quantified.fetch_add(1, Ordering::Relaxed),
            };
            if family == 2 && optimized && !has_apply(&plan.root) {
                decorrelated.fetch_add(1, Ordering::Relaxed);
            }
            let batch_rows = [1, 7, 64, 1024][batch];
            let physical = plan_with(&plan, catalog.as_ref(), PlanOptions { batch_rows })
                .unwrap_or_else(|e| panic!("physical planning failed: {e}\n{}", print_plan(&plan)));
            let mut found = [0; 3];
            lookups(&physical, &mut found);
            for (counter, n) in [point, prefix, secondary].iter().zip(found) {
                counter.fetch_add(n, Ordering::Relaxed);
            }
            let tables = g.data();
            let mut db = Database::new();
            for (id, rows) in (100..).zip(&tables) {
                db.set_rows(TableId::new(id), rows.clone());
            }
            let expected = interpret(
                &plan,
                catalog.as_ref(),
                &mut db,
                &[],
                ExecOptions::default(),
            );
            let actual = runtime.block_on(async {
                let txn = storage.begin(TxnOptions::default()).await.unwrap();
                for (name, rows) in ["a", "b", "c"].iter().zip(&tables) {
                    let def = catalog::table(&txn, name).await.unwrap().unwrap();
                    let stored: Vec<Vec<_>> = rows
                        .iter()
                        .map(|r| {
                            r.iter()
                                .map(|d| interlaken_exec::to_stored(d).unwrap())
                                .collect()
                        })
                        .collect();
                    rows::insert_many(&txn, &def, stored).await.unwrap();
                }
                txn.next_statement();
                let ctx = ExecContext::new(
                    txn.clone(),
                    shared.clone(),
                    Vec::new(),
                    ExecOptions::default(),
                );
                let result = collect(&physical, &ctx).await;
                drop(ctx);
                txn.abort();
                result
            });
            let describe = || format!("{}\nbatch rows {batch_rows}\n{text}", print_plan(&plan));
            match (expected, actual) {
                (Ok(expected), Ok(batches)) => {
                    let got: Vec<Vec<Datum>> = batches
                        .iter()
                        .flat_map(|b| {
                            (0..b.num_rows()).map(move |r| {
                                b.columns()
                                    .iter()
                                    .map(|c| datum_at(c.as_ref(), r).unwrap())
                                    .collect::<Vec<_>>()
                            })
                        })
                        .collect();
                    prop_assert!(
                        bag(&got) == bag(&expected),
                        "rows differ\nexpected {:?}\nactual {:?}\n{}",
                        expected,
                        got,
                        describe()
                    );
                    compared.fetch_add(1, Ordering::Relaxed);
                }
                (Ok(expected), Err(e)) => {
                    return Err(TestCaseError::fail(format!(
                        "executor failed ({e}) where the interpreter returned {expected:?}\n{}",
                        describe()
                    )));
                }
                (Err(_), Err(_)) => {
                    interp_failed.fetch_add(1, Ordering::Relaxed);
                    both_failed.fetch_add(1, Ordering::Relaxed);
                }
                (Err(_), Ok(_)) => {
                    interp_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        },
    )
    .unwrap();
    let [
        compared,
        interp_failed,
        both_failed,
        point,
        prefix,
        secondary,
        decorrelated,
        aggregates,
        quantified,
    ] = counters.map(AtomicU32::into_inner);
    eprintln!(
        "compared {compared} of {cases}; interpreter failed {interp_failed}, executor too in \
         {both_failed}; {aggregates} aggregations, {quantified} quantified ({decorrelated} \
         decorrelated); lookups: {point} point, {prefix} primary prefix, {secondary} secondary"
    );
    assert!(
        compared * 10 >= cases * 8,
        "only {compared} of {cases} compared"
    );
    assert!(
        interp_failed > 0 && both_failed * 2 >= interp_failed,
        "error paths untested"
    );
    assert!(
        point >= 40 && prefix >= 20 && secondary >= 20,
        "too few index lookups"
    );
    assert!(
        decorrelated * 4 >= quantified,
        "too few decorrelated quantified comparisons"
    );
}
