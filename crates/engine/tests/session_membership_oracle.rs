//! The security oracle with memberships changed by DML (milestone F2, ADR-0032).
//!
//! In one transaction, a session interleaves DML on the fixture's three
//! membership edges tables (users in groups, folders in folders, documents
//! in folders) with queries. After every statement, a model recomputes
//! each closure from scratch, by counting paths over its edges, and:
//!
//! - the closure tables must hold exactly the model's pairs and path counts;
//! - each query must return what the unsecured query returns in the
//!   interpreter over only the rows the provider authorizes, with the
//!   model's memberships, and must succeed whenever that does.
//!
//! The policies' atoms include `principal in Group`, `resource in Folder`
//! and `principal.manager in Group`, so authorization depends on the
//! memberships the transaction changes.

#[path = "../../ir/tests/support/security.rs"]
mod security;
#[path = "../../ir/tests/support/security_gen.rs"]
mod security_gen;
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_engine::{Engine, Session};
use interlaken_ir::TableId;
use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Database, Datum, ExecOptions, execute as interpret};
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{EntityUid, RlsProvider, SecValue, SecurityContext};
use interlaken_ir::text::parse_plan;
use interlaken_storage::TxnOptions;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use security::{Data, catalog};
use security_gen::{bag, data, doc_atom, policies_for, query, user_atom, visible};
use support::session::{activate_rls, admin, create, install, replace_rows, rls_text, session};

/// One statement of the transaction.
#[derive(Clone, Debug)]
enum Op {
    /// Add user `.0` to group `.1`.
    Join(i64, i64),
    /// Remove user `.0` from group `.1`.
    Leave(i64, i64),
    /// Put folder `.0` inside folder `.1` (which has a smaller ID, so no cycle).
    Nest(i64, i64),
    /// Take folder `.0` out of folder `.1`.
    Unnest(i64, i64),
    /// File document `.0` in folder `.1`.
    File(i64, i64),
    /// Move every filing of document `.0` to folder `.1`.
    Move(i64, i64),
    /// Remove every filing of document `.0`.
    Unfile(i64),
    /// Run a query.
    Query(String),
}

/// Generate a statement.
fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (1i64..6, 1i64..4).prop_map(|(u, g)| Op::Join(u, g)),
        (1i64..6, 1i64..4).prop_map(|(u, g)| Op::Leave(u, g)),
        (2i64..5, 1i64..4).prop_map(|(c, p)| Op::Nest(c, p.min(c - 1))),
        (2i64..5, 1i64..4).prop_map(|(c, p)| Op::Unnest(c, p.min(c - 1))),
        (10i64..16, 1i64..5).prop_map(|(d, f)| Op::File(d, f)),
        (10i64..16, 1i64..5).prop_map(|(d, f)| Op::Move(d, f)),
        (10i64..16).prop_map(Op::Unfile),
        query().prop_map(Op::Query),
        query().prop_map(Op::Query),
    ]
}

/// The fixture's edges and closure tables, by membership.
const HIERARCHIES: [(&str, &str, &str, &str, TableId, TableId); 3] = [
    (
        "User",
        "Group",
        "user_groups",
        "user_groups_closure",
        TableId::new(204),
        TableId::new(205),
    ),
    (
        "Folder",
        "Folder",
        "folder_parents",
        "folder_closure",
        TableId::new(206),
        TableId::new(207),
    ),
    (
        "Doc",
        "Folder",
        "doc_folders",
        "doc_folders_closure",
        TableId::new(208),
        TableId::new(209),
    ),
];

/// The model: entity data and the edges of each hierarchy.
#[derive(Clone, Debug)]
struct Model {
    /// Entity rows (their membership fields are ignored).
    data: Data,
    /// Direct memberships per hierarchy (index into [`HIERARCHIES`]).
    edges: [BTreeSet<(i64, i64)>; 3],
}

/// One edge between typed entities.
type Edge = ((&'static str, i64), (&'static str, i64));

impl Model {
    /// Start from `data`'s memberships.
    fn new(data: Data) -> Self {
        let users = data.user_groups.iter().copied().collect();
        let folders = data
            .folders
            .iter()
            .filter_map(|(f, p)| p.map(|p| (*f, p)))
            .collect();
        let docs = data
            .docs
            .iter()
            .filter_map(|(d, _, f, ..)| f.map(|f| (*d, f)))
            .collect();
        Self {
            data,
            edges: [users, folders, docs],
        }
    }

    /// Return every closure pair with its path count, recomputed from the
    /// edges by depth-first search, per hierarchy.
    fn closures(&self) -> [BTreeMap<(i64, i64), i64>; 3] {
        let mut graph: Vec<Edge> = Vec::new();
        for (i, (member, group, ..)) in HIERARCHIES.iter().enumerate() {
            graph.extend(
                self.edges[i]
                    .iter()
                    .map(|&(m, g)| ((*member, m), (*group, g))),
            );
        }
        /// Add one to the count of every node reachable from `node` by each path.
        fn walk(
            graph: &[Edge],
            node: (&'static str, i64),
            counts: &mut BTreeMap<(&'static str, i64), i64>,
        ) {
            for (m, g) in graph {
                if *m == node {
                    *counts.entry(*g).or_insert(0) += 1;
                    walk(graph, *g, counts);
                }
            }
        }
        let mut out: [BTreeMap<(i64, i64), i64>; 3] = Default::default();
        let starts: BTreeSet<(&'static str, i64)> = graph.iter().map(|(m, _)| *m).collect();
        for start in starts {
            let mut counts = BTreeMap::new();
            walk(&graph, start, &mut counts);
            for ((ty, a), n) in counts {
                let i = HIERARCHIES
                    .iter()
                    .position(|(m, g, ..)| *m == start.0 && *g == ty)
                    .expect("every reachable pair has a declared membership");
                out[i].insert((start.1, a), n);
            }
        }
        out
    }

    /// Return the database the model describes, closures recomputed from scratch.
    fn database(&self) -> Database {
        let mut db = self.data.database();
        let closures = self.closures();
        for (i, (_, _, _, _, edges, closure)) in HIERARCHIES.iter().enumerate() {
            db.set_rows(
                *edges,
                self.edges[i]
                    .iter()
                    .map(|&(m, g)| vec![Datum::Int8(m), Datum::Int8(g)])
                    .collect(),
            );
            db.set_rows(
                *closure,
                closures[i]
                    .iter()
                    .map(|(&(d, a), &n)| vec![Datum::Int8(d), Datum::Int8(a), Datum::Int8(n)])
                    .collect(),
            );
        }
        db
    }

    /// Return the DML for `op` and apply it to the model, or `None` if it
    /// would fail on a duplicate key (so it is skipped).
    fn apply(&mut self, op: &Op) -> Option<String> {
        let insert = |table: &str, cols: &str, a: i64, b: i64| {
            format!("(plan (insert {table} (values (columns a b) (row {a} {b})) (columns {cols})))")
        };
        let delete = |table: &str, ca: &str, a: i64, cb: Option<(&str, i64)>| match cb {
            Some((cb, b)) => format!(
                "(plan (delete {table} (filter (get {table} :as e) (and (= e.{ca} {a}) (= e.{cb} {b}))) :as e))"
            ),
            None => {
                format!("(plan (delete {table} (filter (get {table} :as e) (= e.{ca} {a})) :as e))")
            }
        };
        Some(match *op {
            Op::Join(u, g) => {
                if !self.edges[0].insert((u, g)) {
                    return None;
                }
                insert("user_groups", "member grp", u, g)
            }
            Op::Leave(u, g) => {
                self.edges[0].remove(&(u, g));
                delete("user_groups", "member", u, Some(("grp", g)))
            }
            Op::Nest(c, p) => {
                if !self.edges[1].insert((c, p)) {
                    return None;
                }
                insert("folder_parents", "child parent", c, p)
            }
            Op::Unnest(c, p) => {
                self.edges[1].remove(&(c, p));
                delete("folder_parents", "child", c, Some(("parent", p)))
            }
            Op::File(d, f) => {
                if !self.edges[2].insert((d, f)) {
                    return None;
                }
                insert("doc_folders", "doc folder", d, f)
            }
            Op::Move(d, f) => {
                let filed: Vec<(i64, i64)> = self.edges[2]
                    .iter()
                    .filter(|(x, _)| *x == d)
                    .copied()
                    .collect();
                // Two filings would both become (d, f): a duplicate key.
                if filed.len() != 1 || filed[0].1 == f {
                    return None;
                }
                self.edges[2].remove(&filed[0]);
                self.edges[2].insert((d, f));
                format!(
                    "(plan (update doc_folders (filter (get doc_folders :as e) (= e.doc {d})) (set folder {f}) :as e))"
                )
            }
            Op::Unfile(d) => {
                self.edges[2].retain(|(x, _)| *x != d);
                delete("doc_folders", "doc", d, None)
            }
            Op::Query(_) => return None,
        })
    }
}

/// Return a closure table's rows read through `session`, as pair counts.
async fn stored_closure(session: &mut Session, table: &str) -> BTreeMap<(i64, i64), i64> {
    let rows = session
        .query(&format!("(plan (get {table}))"), Vec::new())
        .await
        .unwrap()
        .rows();
    rows.iter()
        .map(|r| match (&r[0], &r[1], &r[2]) {
            (Datum::Int8(d), Datum::Int8(a), Datum::Int8(n)) => ((*d, *a), *n),
            other => panic!("unexpected closure row {other:?}"),
        })
        .collect()
}

/// Store the rows of `db` in every fixture table.
async fn store(engine: &Engine, catalog: &InMemoryCatalog, db: &Database) {
    let tables: Vec<(String, Vec<Vec<Datum>>)> = catalog
        .tables()
        .iter()
        .map(|t| (t.name.last().to_string(), db.rows(t.id).to_vec()))
        .collect();
    let tables: Vec<(&str, Vec<Vec<Datum>>)> = tables
        .iter()
        .map(|(n, r)| (n.as_str(), r.clone()))
        .collect();
    replace_rows(engine, &tables).await;
}

#[test]
/// Assert closures maintained by DML match closures recomputed from
/// scratch, and queries later in the same transaction are authorized by
/// the changed memberships.
fn membership_dml_oracle() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let catalog = catalog();
    let engine = runtime.block_on(async {
        let engine = create(dir.path()).await;
        install(&mut session(&engine, admin()), &catalog).await;
        engine
    });
    let cases = 600;
    let counters: [AtomicU32; 3] = Default::default();
    let [queries, nonempty, writes] = &counters;
    let strategy = (
        data(),
        policies_for("Doc", doc_atom()),
        policies_for("User", user_atom()),
        1i64..6,
        proptest::collection::vec(op(), 1..12),
    );
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(&strategy, |(data, mut policies, user_policies, principal, ops)| {
        policies.extend(user_policies);
        let provider = RlsProvider::new(policies.clone());
        let mut context = SecurityContext::new(EntityUid::long("User", principal));
        context.application = vec![("tenant".into(), SecValue::Str("t1".into()))];
        let mut model = Model::new(data);
        let text = rls_text(&policies);
        let data_text = format!("{:?}", model.data);
        let describe = |step: usize| {
            format!(
                "principal {principal}\npolicies {text}\nops {ops:?}\nfailed at step {step}\ndata {data_text}"
            )
        };
        runtime.block_on(async {
            store(&engine, &catalog, &model.database()).await;
            activate_rls(&mut session(&engine, admin()), &text)
                .await
                .unwrap_or_else(|e| panic!("{e}: {text}"));
            let mut s = engine.session(context.clone());
            s.begin(TxnOptions::default()).await.unwrap();
            for (step, op) in ops.iter().enumerate() {
                if let Op::Query(q) = op {
                    let plan = resolve(&parse_plan(q).unwrap(), &catalog).unwrap().plan;
                    let db = model.database();
                    let Ok(expected) = interpret(
                        &plan,
                        &catalog,
                        &mut visible(&db, &catalog, &provider, &context),
                        &[],
                        ExecOptions::default(),
                    ) else {
                        continue;
                    };
                    let actual = s.query(q, Vec::new()).await.map(|r| r.rows()).map_err(|e| {
                        TestCaseError::fail(format!(
                            "the session failed ({e}) where the authorized rows succeed\nquery {q}\n{}",
                            describe(step)
                        ))
                    })?;
                    prop_assert!(
                        bag(expected.clone()) == bag(actual.clone()),
                        "results differ\nquery {}\nexpected {:?}\nactual {:?}\n{}",
                        q,
                        expected,
                        actual,
                        describe(step)
                    );
                    queries.fetch_add(1, Ordering::Relaxed);
                    if !expected.is_empty() {
                        nonempty.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                let Some(dml) = model.apply(op) else { continue };
                s.query(&dml, Vec::new()).await.map_err(|e| {
                    TestCaseError::fail(format!("{dml} failed: {e}\n{}", describe(step)))
                })?;
                writes.fetch_add(1, Ordering::Relaxed);
                let expected = model.closures();
                for (i, (.., table, _, _)) in HIERARCHIES.iter().enumerate() {
                    let actual = stored_closure(&mut s, table).await;
                    prop_assert!(
                        actual == expected[i],
                        "{} differs after {}\nexpected {:?}\nactual {:?}\n{}",
                        table,
                        dml,
                        expected[i],
                        actual,
                        describe(step)
                    );
                }
            }
            s.commit().await.map_err(|e| TestCaseError::fail(format!("commit failed: {e}")))?;
            // The maintained closures are what committed.
            let mut after = session(&engine, admin());
            let expected = model.closures();
            for (i, (.., table, _, _)) in HIERARCHIES.iter().enumerate() {
                prop_assert_eq!(&stored_closure(&mut after, table).await, &expected[i]);
            }
            Ok(())
        })
    })
    .unwrap();
    let [queries, nonempty, writes] = counters.map(|c| c.load(Ordering::Relaxed));
    eprintln!("{queries} queries compared, {nonempty} with rows; {writes} edge writes");
    assert!(queries * 4 >= cases * 3, "too few queries compared");
    assert!(writes >= cases * 2, "too few edge writes");
    assert!(nonempty >= queries / 6, "too few queries returned rows");
}
