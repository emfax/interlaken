//! Security end to end through the executor (ADR-0025, milestone E5).
//!
//! The IR security oracle runs against real storage: for random policies,
//! data, principals and queries, the secured plan (optimized or not),
//! planned physically and executed, must return exactly what the
//! unsecured query returns in the interpreter over only the rows the
//! provider authorizes, and must succeed whenever that does. The leak
//! suite's queries run through the executor too, as do write checks,
//! residual pushdown into storage, the barrier rule on physical plans,
//! and `EXPLAIN`'s hiding of policy logic.

#[path = "../../ir/tests/support/security.rs"]
mod security;
#[path = "../../ir/tests/support/security_gen.rs"]
mod security_gen;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use arrow_array::RecordBatch;
use interlaken_exec::{
    ExecContext, ExecError, ExplainOptions, Op, Physical, PlanOptions, Transaction, collect,
    datum_at, explain, explain_analyze, plan_with, stored_type,
};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Database, Datum, ExecOptions, RowKey, execute as interpret};
use interlaken_ir::optimize::{is_leakproof, optimize};
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{
    DatabaseEntities, EntityUid, RlsProvider, SecValue, SecurityContext, parse_policies,
    print_sec_expr, secure,
};
use interlaken_ir::text::{parse_plan, print_plan};
use interlaken_ir::{Expr, Plan, TableId};
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::Datum as Stored;
use interlaken_storage::{
    Durability, FjallStorage, Storage, StorageOptions, Txn, TxnOptions, rows,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use security::{DOCS, catalog, sample};
use security_gen::{data, doc_atom, policies_for, query, user_atom, visible};

/// Every table of the fixture.
const TABLES: std::ops::RangeInclusive<u32> = 200..=210;

/// Convert an interpreter value to its stored form.
fn stored(d: &Datum) -> Stored {
    match d {
        Datum::Null => Stored::Null,
        Datum::Bool(v) => Stored::Bool(*v),
        Datum::Int8(v) => Stored::Int8(*v),
        Datum::Text(v) => Stored::Text(v.clone()),
        Datum::Array(items) => Stored::Array(items.iter().map(stored).collect()),
        other => panic!("unexpected {other:?}"),
    }
}

/// Storage holding the fixture's tables, empty, plus a runtime to drive it.
struct Fixture {
    /// The runtime.
    runtime: tokio::runtime::Runtime,
    /// The storage.
    storage: FjallStorage,
    /// The IR catalog.
    catalog: Arc<InMemoryCatalog>,
    /// Its storage directory.
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// Create the fixture's tables from the IR catalog.
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let catalog = Arc::new(catalog());
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
            for id in TABLES {
                let def = catalog.table_by_id(TableId::new(id)).unwrap();
                let columns = def
                    .columns
                    .iter()
                    .map(|c| {
                        let column = ColumnDef::new(
                            c.name.as_str(),
                            stored_type(&c.ty.data).expect("storable column"),
                        );
                        if c.ty.nullable {
                            column
                        } else {
                            column.not_null()
                        }
                    })
                    .collect();
                let primary_key = def
                    .primary_key
                    .iter()
                    .map(|k| def.column_by_id(*k).unwrap().name.to_string())
                    .collect();
                let spec = TableSpec {
                    name: def.name.last().to_string(),
                    columns,
                    primary_key,
                };
                catalog::create_table(&txn, spec).await.unwrap();
            }
            txn.commit().await.unwrap();
            storage
        });
        Self {
            runtime,
            storage,
            catalog,
            _dir: dir,
        }
    }

    /// Begin a transaction holding every row of `db`.
    async fn load(&self, db: &Database) -> Txn {
        let txn = self.storage.begin(TxnOptions::default()).await.unwrap();
        for id in TABLES {
            let def = self.catalog.table_by_id(TableId::new(id)).unwrap();
            let stored_def = catalog::table(&txn, def.name.last().as_str())
                .await
                .unwrap()
                .unwrap();
            for row in db.rows(TableId::new(id)) {
                rows::insert(&txn, &stored_def, row.iter().map(stored).collect())
                    .await
                    .unwrap();
            }
        }
        txn.next_statement();
        txn
    }

    /// Return an execution context over `txn`.
    fn context(&self, txn: &Txn) -> ExecContext {
        let shared: Arc<dyn Catalog> = self.catalog.clone();
        ExecContext::new(txn.clone(), shared, Vec::new(), ExecOptions::default())
    }

    /// Run `physical` over the rows of `db` in a rolled-back transaction.
    fn run(&self, physical: &Physical, db: &Database) -> Result<Vec<Vec<Datum>>, ExecError> {
        self.runtime.block_on(async {
            let txn = self.load(db).await;
            let ctx = self.context(&txn);
            let result = collect(physical, &ctx).await;
            drop(ctx);
            txn.abort();
            result.map(|batches| rows_of(&batches))
        })
    }
}

/// Return the rows of `batches`.
fn rows_of(batches: &[RecordBatch]) -> Vec<Vec<Datum>> {
    let mut out = Vec::new();
    for b in batches {
        for r in 0..b.num_rows() {
            out.push(
                b.columns()
                    .iter()
                    .map(|c| datum_at(c.as_ref(), r).unwrap())
                    .collect(),
            );
        }
    }
    out
}

/// Count rows by value.
fn bag(rows: &[Vec<Datum>]) -> HashMap<RowKey, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(RowKey(row.clone())).or_insert(0) += 1;
    }
    counts
}

/// Return the expressions an operator evaluates (not its inputs').
fn exprs(op: &Op) -> Vec<&Expr> {
    match op {
        Op::TableScan { pushed, .. } => pushed.iter().collect(),
        Op::Filter { predicate, .. } => vec![predicate],
        Op::Project { exprs, .. } => exprs.iter().collect(),
        Op::Limit { limit, offset, .. } => limit.iter().chain(offset.iter()).collect(),
        Op::Join { keys, residual, .. } => keys
            .iter()
            .flat_map(|(l, r)| [l, r])
            .chain(residual.iter())
            .collect(),
        Op::Aggregate { keys, aggs, .. } => keys
            .iter()
            .chain(
                aggs.iter()
                    .flat_map(|a| a.args.iter().chain(a.filter.iter())),
            )
            .collect(),
        Op::Sort { keys, .. } => keys.iter().map(|k| &k.expr).collect(),
        Op::Values { rows, .. } => rows.iter().flatten().collect(),
        Op::Unnest { expr, .. } => vec![expr],
        Op::Nest { keys, fields, .. } => keys.iter().chain(fields).collect(),
        Op::WriteCheck { check, .. } => vec![check],
        Op::Update { assignments, .. } => assignments.iter().map(|(_, e)| e).collect(),
        // Policy residuals (in policy filters and scans) are the barrier itself.
        _ => Vec::new(),
    }
}

/// Assert nothing but leakproof expressions runs below any policy filter of `node`.
///
/// A scan's policy conjuncts count as part of the barrier; its user
/// conjuncts are checked like any other expression below one.
fn assert_barriers_hold(node: &Physical, plan: &Plan, catalog: &dyn Catalog, below: bool) {
    if below {
        for e in exprs(&node.op) {
            assert!(
                is_leakproof(e, plan, catalog),
                "non-leakproof {} below a policy filter in\n{}",
                interlaken_ir::text::print_expr(e),
                explain(
                    node,
                    ExplainOptions {
                        show_policies: true
                    }
                )
            );
        }
    }
    let secured = matches!(
        node.op,
        Op::PolicyFilter { .. } | Op::TableScan { secured: true, .. }
    );
    if let Op::TableScan {
        secured: true,
        pushed: Some(e),
        ..
    } = &node.op
    {
        assert!(
            is_leakproof(e, plan, catalog),
            "non-leakproof pushed conjunct"
        );
    }
    for input in node.op.inputs() {
        assert_barriers_hold(input, plan, catalog, below || secured);
    }
}

#[test]
/// Assert secured plans executed over storage return exactly the authorized rows' results.
fn security_oracle_end_to_end() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog.clone();
    let cases = 3000;
    let counters: [AtomicU32; 4] = Default::default();
    let [compared, pushed_policies, policy_filters, multi_batch] = &counters;
    let strategy = (
        data(),
        policies_for("Doc", doc_atom()),
        policies_for("User", user_atom()),
        1i64..7,
        prop_oneof![Just("t1"), Just("t2")],
        query(),
        1usize..4,
    );
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(
        &strategy,
        |(data, mut policies, user_policies, principal, tenant, query, batch_rows)| {
            policies.extend(user_policies);
            let db = data.database();
            let provider = RlsProvider::new(policies.clone());
            let mut context = SecurityContext::new(EntityUid::long("User", principal));
            context.application = vec![("tenant".into(), SecValue::Str(tenant.into()))];
            let plan = resolve(&parse_plan(&query).unwrap(), catalog.as_ref())
                .unwrap()
                .plan;
            let Ok(expected) = interpret(
                &plan,
                catalog.as_ref(),
                &mut visible(&db, &catalog, &provider, &context),
                &[],
                ExecOptions::default(),
            ) else {
                return Ok(());
            };
            let describe = || {
                let shown: Vec<String> = policies
                    .iter()
                    .map(|p| {
                        format!(
                            "{:?} {} {}",
                            p.effect,
                            p.resource_type,
                            print_sec_expr(&p.condition)
                        )
                    })
                    .collect();
                format!(
                    "principal {principal}, tenant {tenant}, batch rows {batch_rows}\npolicies:\n  {}\nquery {query}\ndata {data:?}",
                    shown.join("\n  ")
                )
            };
            let store = DatabaseEntities {
                catalog: catalog.as_ref(),
                db: &db,
            };
            let secured = secure(&plan, catalog.as_ref(), &provider, &context, &store)
                .map_err(|e| TestCaseError::fail(format!("securing failed: {e}\n{}", describe())))?;
            for (label, candidate) in [
                ("secured", secured.clone()),
                ("optimized", optimize(&secured, catalog.as_ref())),
            ] {
                let physical =
                    plan_with(&candidate, catalog.as_ref(), PlanOptions { batch_rows })
                        .map_err(|e| {
                            TestCaseError::fail(format!(
                                "planning failed: {e}\n{}",
                                print_plan(&candidate)
                            ))
                        })?;
                assert_barriers_hold(&physical, &candidate, catalog.as_ref(), false);
                let shape = format!("{physical:?}");
                if shape.contains("policy: Some") {
                    pushed_policies.fetch_add(1, Ordering::Relaxed);
                }
                if shape.contains("PolicyFilter") {
                    policy_filters.fetch_add(1, Ordering::Relaxed);
                }
                let actual = fixture.run(&physical, &db).map_err(|e| {
                    TestCaseError::fail(format!(
                        "{label} plan failed ({e}) where the authorized rows succeed\n{}\n{}",
                        print_plan(&candidate),
                        describe()
                    ))
                })?;
                prop_assert!(
                    bag(&expected) == bag(&actual),
                    "{} results differ\nexpected {:?}\nactual {:?}\n{}\n{}",
                    label,
                    expected,
                    actual,
                    print_plan(&candidate),
                    describe()
                );
                if expected.len() > batch_rows {
                    multi_batch.fetch_add(1, Ordering::Relaxed);
                }
            }
            compared.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    )
    .unwrap();
    let [compared, pushed_policies, policy_filters, multi_batch] =
        counters.map(|c| c.load(Ordering::Relaxed));
    eprintln!(
        "compared {compared} of {cases}; plans with pushed policies {pushed_policies}, with policy filters {policy_filters}, multi-batch {multi_batch}"
    );
    assert!(
        compared * 10 >= cases * 8,
        "only {compared} of {cases} cases compared"
    );
    assert!(
        pushed_policies >= compared / 5,
        "too few pushed policy residuals"
    );
    assert!(policy_filters >= compared / 5, "too few policy filters");
}

/// The leak suite's policies: owners do anything with their documents; users are readable.
const POLICIES: &str = "(policies
    (permit (read insert update delete) Doc (== resource.owner principal))
    (permit (read) User true))";

/// Secure `query` for ann (user 1) under `policies`, optimize it and plan it physically.
fn secured_plan(fixture: &Fixture, policies: &str, db: &Database, query: &str) -> (Plan, Physical) {
    let catalog = fixture.catalog.as_ref();
    let store = DatabaseEntities { catalog, db };
    let provider = RlsProvider::new(parse_policies(policies).unwrap());
    let context = SecurityContext::new(EntityUid::long("User", 1));
    let plan = resolve(&parse_plan(query).unwrap(), catalog)
        .unwrap_or_else(|e| panic!("{e}: {query}"))
        .plan;
    let secured = optimize(
        &secure(&plan, catalog, &provider, &context, &store).unwrap(),
        catalog,
    );
    let physical = plan_with(&secured, catalog, PlanOptions { batch_rows: 2 }).unwrap();
    assert_barriers_hold(&physical, &secured, catalog, false);
    (secured, physical)
}

/// Run `query` secured for ann on the sample data.
fn run(fixture: &Fixture, query: &str) -> Result<Vec<Vec<Datum>>, ExecError> {
    let db = sample().database();
    let (_, physical) = secured_plan(fixture, POLICIES, &db, query);
    fixture.run(&physical, &db)
}

/// Return the first column of each row as integers (NULL as `None`).
fn ints(rows: &[Vec<Datum>]) -> Vec<Option<i64>> {
    rows.iter()
        .map(|r| match r[0] {
            Datum::Int8(v) => Some(v),
            Datum::Null => None,
            ref other => panic!("not an integer: {other:?}"),
        })
        .collect()
}

#[test]
/// Assert the leak suite's queries reveal nothing hidden and raise nothing hidden rows would.
fn leak_suite_end_to_end() {
    let fixture = Fixture::new();
    let run = |q: &str| run(&fixture, q);
    // Document 11 (hidden) has secret 42: 100 / (42 - 42) divides by zero.
    assert_eq!(
        run("(plan (project (filter (get docs :as d) (= (/ 100 (- d.secret 42)) 1)) d.id))")
            .unwrap(),
        Vec::<Vec<Datum>>::new()
    );
    // Document 11's title "bad\" is an invalid LIKE pattern.
    assert!(
        run("(plan (project (filter (get docs :as d) (like \"x\" d.title)) d.id))")
            .unwrap()
            .is_empty()
    );
    // The same in a join condition.
    assert!(
        run("(plan (project (join inner (get docs :as d) (get users :as u) (= (/ 1 (- d.secret 42)) u.id)) d.id))")
            .unwrap()
            .is_empty()
    );
    // Bob owns two hidden documents: a scalar subquery over them sees none.
    let rows = run("(plan (project (filter (get users :as u) (= u.id 2)) (scalar (project (filter (get docs :as d) (= d.owner_id u.id)) d.id))))").unwrap();
    assert_eq!(ints(&rows), [None]);

    let rows = run("(plan (aggregate (get docs :as d) (keys) (aggs (agg count) (agg max d.secret) (agg sum d.secret))))").unwrap();
    assert_eq!(rows, [vec![Datum::Int8(1), Datum::Int8(5), Datum::Int8(5)]]);
    assert_eq!(
        ints(
            &run("(plan (project (limit (sort (get docs :as d) (desc d.secret)) :count 1) d.id))")
                .unwrap()
        ),
        [Some(10)]
    );
    assert_eq!(
        ints(&run("(plan (project (filter (get users :as u) (exists (filter (get docs :as d) (= d.owner_id u.id)))) u.id))").unwrap()),
        [Some(1)]
    );
    // Nested results hold only visible children.
    let nested = run("(plan (project (apply left (get users :as u) (nest (sort (filter (get docs :as d) (= d.owner_id u.id)) (asc d.id)) (keys) (fields d.id d.title) :as docs)) u.id docs))").unwrap();
    let mut nested = nested;
    nested.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(
        nested,
        [
            vec![
                Datum::Int8(1),
                Datum::Array(vec![Datum::Record(vec![
                    Datum::Int8(10),
                    Datum::Text("a plan".into())
                ])])
            ],
            vec![Datum::Int8(2), Datum::Array(vec![])],
            vec![Datum::Int8(3), Datum::Array(vec![])],
        ]
    );
    // Unnesting a visible document's tags; the hidden ones' never appear.
    let tags =
        run("(plan (project (unnest (get docs :as d) d.tags (columns tag)) d.id tag))").unwrap();
    assert_eq!(
        tags,
        [
            vec![Datum::Int8(10), Datum::Text("a".into())],
            vec![Datum::Int8(10), Datum::Text("b".into())],
        ]
    );
}

#[test]
/// Assert writes skip rows the principal can't see and fail, naming only the table, on rows it can't write.
fn writes_are_checked_end_to_end() {
    let fixture = Fixture::new();
    let db = sample().database();
    fixture.runtime.block_on(async {
        let shared: Arc<dyn Catalog> = fixture.catalog.clone();
        let txn = fixture.load(&db).await;
        let mut t = Transaction::new(txn, shared, ExecOptions::default());
        let statement = |q: &str| secured_plan(&fixture, POLICIES, &db, q).1;

        // Updating bob's document changes nothing: ann can't see it.
        let skip = statement("(plan (update docs (filter (get docs :as d) (= d.id 11)) (set title \"mine\") (returning d.id) :as d))");
        assert!(rows_of(&t.execute(&skip, Vec::new()).await.unwrap()).is_empty());

        // Giving her own document to bob fails the check on the new row.
        let give = statement("(plan (update docs (filter (get docs :as d) (= d.id 10)) (set owner_id 2)))");
        let savepoint = t.savepoint().unwrap();
        let err = t.execute(&give, Vec::new()).await.unwrap_err();
        assert_eq!(err.sqlstate(), "42501");
        assert_eq!(err.to_string(), "permission denied for table docs");
        t.rollback_to(savepoint).unwrap();

        // Inserting a document owned by bob fails; one owned by ann succeeds.
        let theirs = statement("(plan (insert docs (values (columns i o t) (row 20 2 \"x\")) (columns id owner_id title)))");
        let savepoint = t.savepoint().unwrap();
        assert_eq!(t.execute(&theirs, Vec::new()).await.unwrap_err().sqlstate(), "42501");
        t.rollback_to(savepoint).unwrap();
        let mine = statement("(plan (insert docs (values (columns i o t) (row 20 1 \"x\")) (columns id owner_id title)))");
        t.execute(&mine, Vec::new()).await.unwrap();

        // Deleting every document removes only ann's.
        let delete = statement("(plan (delete docs (get docs :as d) (returning d.id) :as d))");
        let mut deleted = ints(&rows_of(&t.execute(&delete, Vec::new()).await.unwrap()));
        deleted.sort_unstable();
        assert_eq!(deleted, [Some(10), Some(20)]);
        let stored_docs = catalog::table(t.txn(), "docs").await.unwrap().unwrap();
        t.txn().next_statement();
        let remaining = rows::scan(t.txn(), &stored_docs, &rows::RowScan::all())
            .await
            .unwrap()
            .rows;
        assert_eq!(remaining.len(), 3, "bob's and cy's documents remain");
        t.rollback();
    });
}

#[test]
/// Assert a write check that is NULL (unknown) denies the write, as FALSE does.
fn null_write_checks_deny() {
    let fixture = Fixture::new();
    let db = sample().database();
    let catalog = fixture.catalog.as_ref();
    let mut plan = resolve(
        &parse_plan("(plan (insert docs (values (columns i o t) (row 20 1 \"x\")) (columns id owner_id title)))")
            .unwrap(),
        catalog,
    )
    .unwrap()
    .plan;
    let interlaken_ir::Rel::Insert { input, .. } = &mut plan.root else {
        panic!("expected an insert")
    };
    let unknown = Expr::Cast {
        expr: Box::new(Expr::Literal(interlaken_ir::Value::Null)),
        to: interlaken_ir::text::parse_type("bool").unwrap().data,
    };
    **input = interlaken_ir::Rel::WriteCheck {
        input: input.clone(),
        check: unknown,
    };
    let physical = plan_with(&plan, catalog, PlanOptions::default()).unwrap();
    let err = fixture.run(&physical, &db).unwrap_err();
    assert!(
        matches!(&err, ExecError::PermissionDenied { table } if table == "docs"),
        "{err}"
    );
}

#[test]
/// Assert single-row policy conjuncts run in storage, so hidden rows never leave the storage pool.
fn policy_residuals_run_in_storage() {
    let fixture = Fixture::new();
    let db = sample().database();
    let (_, physical) = secured_plan(
        &fixture,
        POLICIES,
        &db,
        "(plan (project (get docs :as d) d.id))",
    );
    let Op::Project { input, .. } = &physical.op else {
        panic!("expected a projection")
    };
    assert!(
        matches!(
            &input.op,
            Op::TableScan {
                policy: Some(_),
                secured: true,
                ..
            }
        ),
        "the owner check is pushed into the scan, leaving no policy filter:\n{}",
        explain(
            &physical,
            ExplainOptions {
                show_policies: true
            }
        )
    );
    let analyzed = fixture.runtime.block_on(async {
        let txn = fixture.load(&db).await;
        let ctx = fixture.context(&txn);
        let out = explain_analyze(&physical, &ctx, ExplainOptions::default()).await;
        drop(ctx);
        txn.abort();
        out.unwrap()
    });
    // Four documents are stored; only ann's leaves storage.
    let scan_line = analyzed
        .lines()
        .find(|l| l.contains("TableScan docs"))
        .unwrap();
    assert!(scan_line.contains("rows=1 "), "{analyzed}");
}

#[test]
/// Assert `EXPLAIN` shows where policy filters sit but hides their logic unless allowed.
fn explain_hides_policy_logic() {
    let fixture = Fixture::new();
    let db = sample().database();
    let policies = "(policies
        (permit (read) Doc (and (in resource (entity Folder 1)) (contains resource.tags \"quokka\")))
        (permit (read) Doc (== resource.owner.dept \"zebra\"))
        (permit (read) Doc (== resource.secret 4242))
        (permit (read) User true))";
    for query in [
        "(plan (project (get docs :as d) d.id))",
        "(plan (project (filter (get docs :as d) (like d.title \"%plan%\")) d.id))",
        "(plan (project (join inner (get docs :as d) (get users :as u) (= d.owner_id u.id)) d.id u.name))",
    ] {
        let (_, physical) = secured_plan(&fixture, policies, &db, query);
        let hidden = explain(&physical, ExplainOptions::default());
        let shown = explain(
            &physical,
            ExplainOptions {
                show_policies: true,
            },
        );
        for secret in ["quokka", "zebra", "4242", "closure", "sec$"] {
            assert!(!hidden.contains(secret), "{secret} visible in\n{hidden}");
        }
        assert!(
            hidden.contains("PolicyFilter(docs)") || hidden.contains("policy: <policy>"),
            "{hidden}"
        );
        assert!(
            shown.contains("quokka") && shown.contains("4242"),
            "{shown}"
        );
        // The user's own predicate stays visible.
        if query.contains("like") {
            assert!(hidden.contains("%plan%"), "{hidden}");
        }
    }
}

#[test]
/// Assert write checks hide their condition in `EXPLAIN` and name the target table.
fn explain_hides_write_checks() {
    let fixture = Fixture::new();
    let db = sample().database();
    let (_, physical) = secured_plan(
        &fixture,
        "(policies (permit (read) Doc true) (permit (insert) Doc (== resource.secret 31337)))",
        &db,
        "(plan (insert docs (project (get docs :as d) (+ d.id 100) d.owner_id d.title d.secret) (columns id owner_id title secret)))",
    );
    let hidden = explain(&physical, ExplainOptions::default());
    assert!(
        hidden.contains("WriteCheck(docs)") && !hidden.contains("31337"),
        "{hidden}"
    );
    let shown = explain(
        &physical,
        ExplainOptions {
            show_policies: true,
        },
    );
    assert!(shown.contains("31337"), "{shown}");
    assert_eq!(DOCS, TableId::new(203));
}

#[test]
/// Assert the subplans a write check's policy adds are hidden, not just its condition.
fn explain_hides_write_check_subplans() {
    let fixture = Fixture::new();
    let db = sample().database();
    let (_, physical) = secured_plan(
        &fixture,
        "(policies (permit (read) Doc true) (permit (insert) Doc (in resource (entity Folder 77))))",
        &db,
        "(plan (insert docs (project (filter (get docs :as d) (> d.id 1)) (+ d.id 100) d.owner_id d.title d.secret) (columns id owner_id title secret)))",
    );
    let shown = explain(
        &physical,
        ExplainOptions {
            show_policies: true,
        },
    );
    let hidden = explain(&physical, ExplainOptions::default());
    // The user's own query stays visible; the policy's lookup does not.
    assert!(hidden.contains("WriteCheck(docs)"), "{hidden}");
    assert!(hidden.contains("TableScan docs"), "{hidden}");
    assert!(hidden.contains("> #"), "{hidden}");
    for secret in ["77", "sec$"] {
        assert!(!hidden.contains(secret), "{secret} visible in\n{hidden}");
    }
    assert!(shown.contains("77"), "{shown}");
}

/// Swap the inputs of every join whose right side security lowering added.
fn swap_policy_joins(node: &mut Physical) {
    if let Op::Join { left, right, .. } = &mut node.op
        && matches!(
            right.op,
            Op::TableScan {
                policy_added: true,
                ..
            } | Op::Distinct { .. }
        )
    {
        std::mem::swap(left, right);
    }
    match &mut node.op {
        Op::Join { left, right, .. } => {
            swap_policy_joins(left);
            swap_policy_joins(right);
        }
        Op::Apply { outer, inner, .. } => {
            swap_policy_joins(outer);
            swap_policy_joins(inner);
        }
        Op::Filter { input, .. }
        | Op::PolicyFilter { input, .. }
        | Op::Project { input, .. }
        | Op::Distinct { input } => swap_policy_joins(input),
        _ => {}
    }
}

#[test]
/// Assert redaction follows the policy marker, not the plan's shape: a
/// policy subplan on the outer side of a join is hidden too.
fn explain_hides_reshaped_policy_subplans() {
    let fixture = Fixture::new();
    let db = sample().database();
    let (_, mut physical) = secured_plan(
        &fixture,
        "(policies (permit (read) Doc (in resource (entity Folder 4242))))",
        &db,
        "(plan (project (get docs :as d) d.id))",
    );
    swap_policy_joins(&mut physical);
    let hidden = explain(&physical, ExplainOptions::default());
    assert!(hidden.contains("TableScan docs"), "{hidden}");
    for secret in ["4242", "closure", "sec$"] {
        assert!(!hidden.contains(secret), "{secret} visible in\n{hidden}");
    }
}

#[test]
/// Assert client plans can't forge the policy marker.
fn clients_cannot_use_the_policy_marker() {
    use interlaken_ir::verify::{VerifyOptions, verify};
    let fixture = Fixture::new();
    let catalog = fixture.catalog.as_ref();
    for query in [
        "(plan (project (get docs :as sec$1) sec$1.id))",
        "(plan (values (columns sec$2) (row 1)))",
    ] {
        let plan = resolve(&parse_plan(query).unwrap(), catalog)
            .unwrap_or_else(|e| panic!("{e}: {query}"))
            .plan;
        let err = verify(&plan, catalog, &VerifyOptions { trusted: false }).unwrap_err();
        assert!(err.to_string().contains("reserved prefix"), "{err}");
    }
}

#[test]
/// Assert the subplans lowering adds are hidden in unoptimized plans too,
/// where the optimizer hasn't pruned their columns.
fn explain_hides_unoptimized_policy_subplans() {
    let fixture = Fixture::new();
    let db = sample().database();
    let catalog = fixture.catalog.as_ref();
    let store = DatabaseEntities { catalog, db: &db };
    let provider = RlsProvider::new(
        parse_policies(
            "(policies (permit (read) Doc true) (permit (insert) Doc (contains resource.tags \"quokka\")))",
        )
        .unwrap(),
    );
    let context = SecurityContext::new(EntityUid::long("User", 1));
    let query = "(plan (insert docs (project (get docs :as d) (+ d.id 100) d.owner_id d.title d.tags) (columns id owner_id title tags)))";
    let plan = resolve(&parse_plan(query).unwrap(), catalog).unwrap().plan;
    let secured = secure(&plan, catalog, &provider, &context, &store).unwrap();
    let physical = plan_with(&secured, catalog, PlanOptions::default()).unwrap();
    let hidden = explain(&physical, ExplainOptions::default());
    assert!(hidden.contains("WriteCheck(docs)"), "{hidden}");
    assert!(hidden.contains("<policy> subplan"), "{hidden}");
    assert!(!hidden.contains("quokka"), "{hidden}");
}
