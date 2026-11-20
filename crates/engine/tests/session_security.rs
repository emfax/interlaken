//! Security through the engine's public API (milestone F1, ADR-0031).
//!
//! The fixture's catalog is created by engine DDL, policies are activated
//! through verified proposals, and queries are IR text run by a session
//! for the principal. For random policies, data, principals and queries,
//! a session's results must equal what the unsecured query returns in the
//! interpreter over only the rows the provider authorizes, and the session
//! must succeed whenever that does. The leak suite runs through the API too.

#[path = "../../ir/tests/support/security.rs"]
mod security;
#[path = "../../ir/tests/support/security_gen.rs"]
mod security_gen;
mod support;

use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_engine::Engine;
use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Database, Datum, ExecOptions, execute as interpret};
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{EntityUid, Policy, RlsProvider, SecValue, SecurityContext};
use interlaken_ir::text::parse_plan;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use security::{catalog, sample};
use security_gen::{bag, data, doc_atom, policies_for, query, user_atom, visible};
use support::session::{activate_rls, admin, create, install, replace_rows, rls_text, session};

/// The engine with the fixture's catalog, and a runtime to drive it.
struct Fixture {
    /// The runtime.
    runtime: tokio::runtime::Runtime,
    /// The engine.
    engine: Engine,
    /// The fixture's IR catalog, for the expected side.
    catalog: InMemoryCatalog,
    /// The storage directory.
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// Create the fixture's tables, entity types and memberships through engine DDL.
    fn new() -> Self {
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
        Self {
            runtime,
            engine,
            catalog,
            _dir: dir,
        }
    }

    /// Store the rows of `db` in every fixture table and activate `policies`.
    fn prepare(&self, db: &Database, policies: &[Policy]) {
        let tables: Vec<(String, Vec<Vec<Datum>>)> = self
            .catalog
            .tables()
            .iter()
            .map(|t| (t.name.last().to_string(), db.rows(t.id).to_vec()))
            .collect();
        let tables: Vec<(&str, Vec<Vec<Datum>>)> = tables
            .iter()
            .map(|(name, rows)| (name.as_str(), rows.clone()))
            .collect();
        let text = rls_text(policies);
        self.runtime.block_on(async {
            replace_rows(&self.engine, &tables).await;
            activate_rls(&mut session(&self.engine, admin()), &text)
                .await
                .unwrap_or_else(|e| panic!("{e}: {text}"));
        });
    }

    /// Run `query` in a session for `context`.
    fn run(
        &self,
        context: SecurityContext,
        query: &str,
    ) -> Result<Vec<Vec<Datum>>, interlaken_engine::EngineError> {
        self.runtime.block_on(async {
            let mut session = self.engine.session(context);
            session.query(query, Vec::new()).await.map(|r| r.rows())
        })
    }
}

#[test]
/// Assert sessions return exactly the authorized rows' results, and never
/// fail where those succeed.
fn security_oracle_through_the_api() {
    let fixture = Fixture::new();
    let cases = 1000;
    let counters: [AtomicU32; 2] = Default::default();
    let [compared, nonempty] = &counters;
    let strategy = (
        data(),
        policies_for("Doc", doc_atom()),
        policies_for("User", user_atom()),
        1i64..7,
        prop_oneof![Just("t1"), Just("t2")],
        query(),
    );
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(
        &strategy,
        |(data, mut policies, user_policies, principal, tenant, query)| {
            policies.extend(user_policies);
            let db = data.database();
            let provider = RlsProvider::new(policies.clone());
            let mut context = SecurityContext::new(EntityUid::long("User", principal));
            context.application = vec![("tenant".into(), SecValue::Str(tenant.into()))];
            let plan = resolve(&parse_plan(&query).unwrap(), &fixture.catalog)
                .unwrap()
                .plan;
            let Ok(expected) = interpret(
                &plan,
                &fixture.catalog,
                &mut visible(&db, &fixture.catalog, &provider, &context),
                &[],
                ExecOptions::default(),
            ) else {
                return Ok(());
            };
            fixture.prepare(&db, &policies);
            let describe = || {
                format!(
                    "principal {principal}, tenant {tenant}\npolicies {}\nquery {query}\ndata {data:?}",
                    rls_text(&policies)
                )
            };
            let actual = fixture.run(context, &query).map_err(|e| {
                TestCaseError::fail(format!(
                    "the session failed ({e}) where the authorized rows succeed\n{}",
                    describe()
                ))
            })?;
            prop_assert!(
                bag(expected.clone()) == bag(actual.clone()),
                "results differ\nexpected {:?}\nactual {:?}\n{}",
                expected,
                actual,
                describe()
            );
            compared.fetch_add(1, Ordering::Relaxed);
            if !expected.is_empty() {
                nonempty.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        },
    )
    .unwrap();
    let [compared, nonempty] = counters.map(|c| c.load(Ordering::Relaxed));
    eprintln!("compared {compared} of {cases}, {nonempty} with rows");
    assert!(
        compared * 10 >= cases * 8,
        "only {compared} of {cases} cases compared"
    );
    // Atoms over entities beyond the principal (F2) are often false, so a
    // fifth rather than a quarter of the cases return rows.
    assert!(nonempty >= compared / 5, "too few cases returned rows");
}

/// The leak suite's policies: owners do anything with their documents; users are readable.
const POLICIES: &str = "(policies
    (permit (read insert update delete) Doc (== resource.owner principal))
    (permit (read) User true))";

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
/// Assert rows a principal can't see never leak through errors,
/// aggregates, ordering, subqueries, nesting or unnesting, through the API.
fn leak_suite_through_the_api() {
    let fixture = Fixture::new();
    let db = sample().database();
    let policies = interlaken_ir::security::parse_policies(POLICIES).unwrap();
    fixture.prepare(&db, &policies);
    let run = |q: &str| {
        fixture
            .run(SecurityContext::new(EntityUid::long("User", 1)), q)
            .unwrap_or_else(|e| panic!("{e}: {q}"))
    };
    // Document 11 (hidden) has secret 42: 100 / (42 - 42) divides by zero.
    assert!(
        run("(plan (project (filter (get docs :as d) (= (/ 100 (- d.secret 42)) 1)) d.id))")
            .is_empty()
    );
    // Document 11's title "bad\" is an invalid LIKE pattern.
    assert!(run("(plan (project (filter (get docs :as d) (like \"x\" d.title)) d.id))").is_empty());
    assert!(
        run("(plan (project (join inner (get docs :as d) (get users :as u) (= (/ 1 (- d.secret 42)) u.id)) d.id))")
            .is_empty()
    );
    let rows = run(
        "(plan (project (filter (get users :as u) (= u.id 2)) (scalar (project (filter (get docs :as d) (= d.owner_id u.id)) d.id))))",
    );
    assert_eq!(ints(&rows), [None]);
    let rows = run(
        "(plan (aggregate (get docs :as d) (keys) (aggs (agg count) (agg max d.secret) (agg sum d.secret))))",
    );
    assert_eq!(rows, [vec![Datum::Int8(1), Datum::Int8(5), Datum::Int8(5)]]);
    assert_eq!(
        ints(&run(
            "(plan (project (limit (sort (get docs :as d) (desc d.secret)) :count 1) d.id))"
        )),
        [Some(10)]
    );
    assert_eq!(
        ints(&run(
            "(plan (project (filter (get users :as u) (exists (filter (get docs :as d) (= d.owner_id u.id)))) u.id))"
        )),
        [Some(1)]
    );
    let mut nested = run(
        "(plan (project (apply left (get users :as u) (nest (sort (filter (get docs :as d) (= d.owner_id u.id)) (asc d.id)) (keys) (fields d.id d.title) :as docs)) u.id docs))",
    );
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
    assert_eq!(
        run("(plan (project (unnest (get docs :as d) d.tags (columns tag)) d.id tag))"),
        [
            vec![Datum::Int8(10), Datum::Text("a".into())],
            vec![Datum::Int8(10), Datum::Text("b".into())],
        ]
    );
    // Writes skip invisible rows, and fail on rows the principal may not write.
    let ctx = || SecurityContext::new(EntityUid::long("User", 1));
    let deleted = fixture
        .run(
            ctx(),
            "(plan (delete docs (get docs :as d) (returning d.id) :as d))",
        )
        .unwrap();
    assert_eq!(ints(&deleted), [Some(10)]);
    let err = fixture
        .run(
            ctx(),
            "(plan (insert docs (values (columns id owner_id title) (row 99 2 \"x\")) (columns id owner_id title)))",
        )
        .unwrap_err();
    assert_eq!(err.sqlstate(), "42501", "{err}");
}
