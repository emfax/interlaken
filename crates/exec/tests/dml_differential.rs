//! Differential tests of DML and statement semantics against the reference
//! interpreter (ADR-0026, milestone E4).
//!
//! Each case runs a random sequence of statements — inserts of literal rows
//! and of query results, updates (some driven by joins that name a target
//! row several times), deletes (some with subqueries), all with optional
//! `RETURNING`, and queries — through the interpreter and through the
//! executor's statement runner on real storage with secondary indexes, so
//! filters on indexed columns use index scans. After every statement the
//! results and the full contents of both tables must agree, and a statement
//! must fail in one exactly when it fails in the other; a failed statement
//! leaves nothing behind in either, and the executor recovers from the
//! aborted state by rolling back to a savepoint.
//!
//! Updates never assign primary key or unique columns: the interpreter
//! checks uniqueness at the end of a statement, storage after each row (as
//! Postgres does for non-deferrable constraints), so the two legitimately
//! differ when one statement swaps or shifts unique values.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_exec::{Transaction, datum_at, plan};
use interlaken_ir::TableId;
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Database, Datum, ExecOptions, RowKey, execute as interpret};
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan, print_plan};
use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableSpec};
use interlaken_storage::encoding::{ColumnType, SortOrder};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

/// The IR catalog, with the same indexes as storage.
const CATALOG: &str = "(catalog (builtins)
  (table users :id 100
    (column id int8) (column name (option text)) (column age (option int4)) (column karma int8)
    (primary-key id)
    (index users_age age)
    (index users_name name :unique true))
  (table posts :id 101
    (column id int8) (column author (option int8)) (column likes (option int4)) (column title (option text))
    (primary-key id)
    (index posts_author author)
    (index posts_likes (desc likes))))";

/// A deterministic generator (xorshift64*).
struct Gen(u64);

impl Gen {
    /// Return the next random number.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Return a number below `n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Return true with probability `percent`/100.
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// Return one of `items`.
    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[usize::try_from(self.below(items.len() as u64)).unwrap()].clone()
    }

    /// Return an int4 literal or NULL.
    fn int4(&mut self) -> String {
        if self.chance(15) {
            "(cast null int4)".into()
        } else {
            format!("(int4 {})", self.below(5))
        }
    }

    /// Return a name literal or NULL.
    fn name(&mut self) -> String {
        if self.chance(15) {
            "(cast null text)".into()
        } else {
            format!("\"n{}\"", self.below(8))
        }
    }

    /// Return a predicate over the user columns under `q` (`q.id`, `q.age`, ...).
    fn user_predicate(&mut self, q: &str) -> String {
        match self.below(7) {
            0 => format!("(= {q}.age {})", self.int4()),
            1 => format!(
                "(and (>= {q}.age (int4 {})) (< {q}.age (int4 {})))",
                self.below(3),
                1 + self.below(5)
            ),
            2 => format!("(< {q}.id {})", self.below(8)),
            3 => format!("(= {q}.name {})", self.name()),
            4 => format!("(exists (filter (get posts :as pp) (= pp.author {q}.id)))"),
            5 => format!("(> (/ (int4 6) {q}.age) (int4 1))"),
            _ => format!("(is-null {q}.age)"),
        }
    }

    /// Return a predicate over the post columns under `q`.
    fn post_predicate(&mut self, q: &str) -> String {
        match self.below(6) {
            0 => format!("(= {q}.author {})", self.below(6)),
            1 => format!("(> {q}.likes {})", self.int4()),
            2 => format!(
                "(in {q}.author (project (filter (get users :as uu) (> uu.karma 1)) uu.id))"
            ),
            3 => format!("(<= {q}.id {})", 100 + self.below(8)),
            4 => format!("(= (% {q}.likes (int4 2)) (int4 0))"),
            _ => "true".into(),
        }
    }

    /// Return a random statement.
    fn statement(&mut self) -> String {
        let returning = |g: &mut Gen, cols: &str| {
            if g.chance(40) {
                format!(" (returning {cols})")
            } else {
                String::new()
            }
        };
        match self.below(10) {
            9 => {
                // Posts by random authors, so authors often have several.
                let rows: String = (0..1 + self.below(3))
                    .map(|_| {
                        format!(
                            " (row {} {} {} \"p\")",
                            100 + self.below(12),
                            self.below(6),
                            self.int4()
                        )
                    })
                    .collect();
                format!(
                    "(insert posts (values (columns i a l t){rows}) (columns id author likes title))"
                )
            }
            0 | 1 => {
                let rows: String = (0..1 + self.below(3))
                    .map(|_| {
                        let id = if self.chance(5) {
                            "(cast null int8)".to_owned()
                        } else {
                            format!("{}", self.below(8))
                        };
                        format!(
                            " (row {id} {} {} {})",
                            self.name(),
                            self.int4(),
                            self.below(4)
                        )
                    })
                    .collect();
                let r = returning(self, "id (concat name \"!\")");
                format!(
                    "(insert users (values (columns i n a k){rows}) (columns id name age karma){r})"
                )
            }
            2 => {
                let p = self.user_predicate("u");
                let r = returning(self, "id author");
                format!(
                    "(insert posts (project (filter (get users :as u) {p}) (as (+ u.id {}) i) (as u.id a) (as u.age l) (as u.name t)) (columns id author likes title){r})",
                    100 + self.below(4)
                )
            }
            3 | 4 => {
                let p = self.user_predicate("u");
                let age = self.pick(&[
                    "(+ u.age (int4 1))",
                    "(int4 0)",
                    "(cast null int4)",
                    "(/ (int4 12) u.age)",
                    "(coalesce u.age (int4 9))",
                ]);
                let r = returning(self, "u.id u.age u.karma");
                format!(
                    "(update users :as u (filter (get users :as u) {p}) (set age {age}) (set karma (+ u.karma 1)){r})"
                )
            }
            5 => {
                // A join names each author once per post; each is updated once.
                let r = returning(self, "u.id u.karma");
                format!(
                    "(update users :as u (join inner (get users :as u) (get posts :as p) (= p.author u.id)) (set karma (* u.karma 2)){r})"
                )
            }
            6 => {
                let p = self.post_predicate("p");
                let r = returning(self, "p.id p.title");
                format!("(delete posts :as p (filter (get posts :as p) {p}){r})")
            }
            7 => {
                let p = self.user_predicate("u");
                format!("(delete users :as u (filter (get users :as u) {p}))")
            }
            _ => {
                let p = self.post_predicate("p");
                format!("(filter (get posts :as p) {p})")
            }
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

#[test]
/// Assert statement sequences have the same effects and results in the executor and the interpreter.
fn statements_agree_with_interpreter() {
    agree(CATALOG);
}

#[test]
/// Assert the same with a foreign key from posts to their authors (ADR-0035):
/// posts by missing authors and deletes of users with posts fail with
/// `23503` in both, and statements that satisfy it agree.
fn statements_agree_with_interpreter_under_a_foreign_key() {
    let catalog = CATALOG.strip_suffix(')').unwrap().to_owned()
        + "\n  (foreign-key posts_author_fk :table posts (columns author) (references users id)))";
    let violations = agree(&catalog);
    assert!(violations > 0, "no statement violated the foreign key");
}

/// Run random statement sequences against `catalog_text` through both, and
/// return how many failed in both with a foreign key violation.
fn agree(catalog_text: &str) -> u32 {
    agree_counting(catalog_text, &[]).0
}

#[test]
/// Assert the same with the foreign key cascading (ADR-0035): deleting a
/// user deletes their posts, in both.
fn statements_agree_with_interpreter_under_a_cascading_foreign_key() {
    let catalog = CATALOG.strip_suffix(')').unwrap().to_owned()
        + "\n  (foreign-key posts_author_fk :table posts (columns author) (references users id)\n    :on-delete cascade))";
    // Users 0 to 5 exist from the start, so posts by them are accepted and
    // deleting a user often cascades.
    let users = "(insert users (values (columns i n a k) (row 0 \"a\" (int4 1) 1) \
                 (row 1 \"b\" (int4 2) 1) (row 2 \"c\" (int4 3) 1) (row 3 \"d\" (int4 4) 1) \
                 (row 4 \"e\" (cast null int4) 1) (row 5 \"f\" (int4 0) 1)) \
                 (columns id name age karma))";
    let (_, cascaded) = agree_counting(&catalog, &[users]);
    // Over seven runs, 49 to 73 of 600 cases cascaded (mean 63, sd 10); 20
    // is over four standard deviations below, so it guards the generator,
    // not chance.
    assert!(cascaded >= 20, "only {cascaded} deletes cascaded");
}

/// Run as [`agree`], with `setup` run first in each case; also return how
/// many statements deleted users with posts.
fn agree_counting(catalog_text: &str, setup: &[&str]) -> (u32, u32) {
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
        let tables = [
            (
                "users",
                vec![
                    ("id", ColumnType::Int8),
                    ("name", ColumnType::Text),
                    ("age", ColumnType::Int4),
                    ("karma", ColumnType::Int8),
                ],
            ),
            (
                "posts",
                vec![
                    ("id", ColumnType::Int8),
                    ("author", ColumnType::Int8),
                    ("likes", ColumnType::Int4),
                    ("title", ColumnType::Text),
                ],
            ),
        ];
        for (name, columns) in tables {
            let columns = columns
                .into_iter()
                .map(|(c, ty)| {
                    if matches!(c, "id" | "karma") {
                        ColumnDef::new(c, ty).not_null()
                    } else {
                        ColumnDef::new(c, ty)
                    }
                })
                .collect();
            catalog::create_table(
                &txn,
                TableSpec {
                    name: name.into(),
                    columns,
                    primary_key: vec!["id".into()],
                },
            )
            .await
            .unwrap();
        }
        for (name, table, column, order, unique) in [
            ("users_age", "users", "age", SortOrder::ASC, false),
            ("users_name", "users", "name", SortOrder::ASC, true),
            ("posts_author", "posts", "author", SortOrder::ASC, false),
            ("posts_likes", "posts", "likes", SortOrder::DESC, false),
        ] {
            let spec = IndexSpec {
                name: name.into(),
                table: table.into(),
                columns: vec![(column.into(), order)],
                unique,
                nulls_distinct: true,
            };
            catalog::create_index(&txn, spec).await.unwrap();
        }
        txn.commit().await.unwrap();
        storage
    });
    let catalog = Arc::new(parse_catalog(catalog_text).unwrap());
    let shared: Arc<dyn Catalog> = catalog.clone();
    let cases = 600;
    let counters: [AtomicU32; 5] = Default::default();
    let [statements, failures, index_scans, violations, cascaded] = &counters;
    // Cascades run unsecured here, as there are no policies.
    let cascades = Arc::new(
        interlaken_exec::cascade_plans(
            &resolve(
                &parse_plan("(plan (delete users (get users)))").unwrap(),
                catalog.as_ref(),
            )
            .unwrap()
            .plan,
            catalog.as_ref(),
        )
        .unwrap(),
    );
    let parse = |text: &str, catalog: &InMemoryCatalog| {
        resolve(&parse_plan(&format!("(plan {text})")).unwrap(), catalog)
            .unwrap_or_else(|e| panic!("{e}: {text}"))
            .plan
    };
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(&any::<u64>(), |seed| {
        let mut g = Gen(seed | 1);
        let mut db = Database::new();
        let mut txn = runtime.block_on(async {
            Transaction::new(
                storage.begin(TxnOptions::default()).await.unwrap(),
                shared.clone(),
                ExecOptions::default(),
            )
            .with_cascades(cascades.clone())
        });
        let mut history = Vec::new();
        for i in 0..setup.len() + 8 {
            let text = setup
                .get(i)
                .map_or_else(|| g.statement(), |s| (*s).to_owned());
            history.push(text.clone());
            let ir = parse(&text, &catalog);
            let physical =
                plan(&ir, catalog.as_ref()).unwrap_or_else(|e| panic!("{e}\n{}", print_plan(&ir)));
            if format!("{physical:?}").contains("Index {") {
                index_scans.fetch_add(1, Ordering::Relaxed);
            }
            let posts_before = db.rows(TableId::new(101)).len();
            let expected = interpret(&ir, catalog.as_ref(), &mut db, &[], ExecOptions::default());
            if text.starts_with("(delete users")
                && expected.is_ok()
                && db.rows(TableId::new(101)).len() < posts_before
            {
                cascaded.fetch_add(1, Ordering::Relaxed);
            }
            let actual = runtime.block_on(async {
                let savepoint = txn.savepoint().unwrap();
                let result = txn.execute(&physical, Vec::new()).await;
                if result.is_err() {
                    txn.rollback_to(savepoint).unwrap();
                } else {
                    txn.release(savepoint).unwrap();
                }
                result
            });
            statements.fetch_add(1, Ordering::Relaxed);
            let describe = || format!("statements:\n{}", history.join("\n"));
            match (&expected, &actual) {
                (Ok(e), Ok(batches)) => {
                    let got: Vec<Vec<Datum>> = batches
                        .iter()
                        .flat_map(|b| {
                            (0..b.num_rows()).map(move |r| {
                                b.columns()
                                    .iter()
                                    .map(|c| datum_at(c.as_ref(), r).unwrap())
                                    .collect()
                            })
                        })
                        .collect();
                    prop_assert!(
                        bag(&got) == bag(e),
                        "results differ\nexpected {:?}\nactual {:?}\n{}",
                        e,
                        got,
                        describe()
                    );
                }
                (Err(e), Err(a)) => {
                    failures.fetch_add(1, Ordering::Relaxed);
                    let foreign = [e.sqlstate(), a.sqlstate()].map(|s| s == "23503");
                    prop_assert!(
                        foreign[0] == foreign[1],
                        "interpreter {e}, executor {a}\n{}",
                        describe()
                    );
                    if foreign[0] {
                        violations.fetch_add(1, Ordering::Relaxed);
                    }
                }
                (e, a) => {
                    return Err(TestCaseError::fail(format!(
                        "interpreter {e:?}, executor {:?}\n{}",
                        a.as_ref().map(|b| b.len()),
                        describe()
                    )));
                }
            }
            // The tables must hold the same rows.
            for (name, id) in [("users", 100), ("posts", 101)] {
                let all =
                    plan(&parse(&format!("(get {name})"), &catalog), catalog.as_ref()).unwrap();
                let stored = runtime.block_on(txn.execute(&all, Vec::new())).unwrap();
                let got: Vec<Vec<Datum>> = stored
                    .iter()
                    .flat_map(|b| {
                        (0..b.num_rows()).map(move |r| {
                            b.columns()
                                .iter()
                                .map(|c| datum_at(c.as_ref(), r).unwrap())
                                .collect()
                        })
                    })
                    .collect();
                let want = db.rows(TableId::new(id)).to_vec();
                prop_assert!(
                    bag(&got) == bag(&want),
                    "{} differs\nexpected {:?}\nactual {:?}\n{}",
                    name,
                    want,
                    got,
                    describe()
                );
            }
        }
        txn.rollback();
        Ok(())
    })
    .unwrap();
    let [statements, failures, index_scans, violations, cascaded] =
        counters.map(|c| c.into_inner());
    eprintln!(
        "{statements} statements, {failures} failed in both ({violations} on a foreign key), \
         {index_scans} used an index, {cascaded} deletes cascaded"
    );
    assert!(
        failures * 20 >= statements,
        "too few failing statements: {failures}"
    );
    assert!(
        index_scans * 10 >= statements,
        "too few index scans: {index_scans}"
    );
    (violations, cascaded)
}
