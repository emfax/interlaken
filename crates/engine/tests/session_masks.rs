//! Column masking through the engine's public API (milestone F2, ADR-0032).
//!
//! Column policies that depend on the row and declare a mask show masked
//! values instead of denying the column. For random row policies, column
//! policies with masks, data, principals and queries, a session's results
//! must equal what the unsecured query returns in the interpreter over the
//! rows the provider authorizes, with each masked column replaced, row by
//! row, wherever its column policies (decided concretely) deny the row.

#[path = "../../ir/tests/support/security.rs"]
mod security;
#[path = "../../ir/tests/support/security_gen.rs"]
mod security_gen;
mod support;

use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_engine::Engine;
use interlaken_ir::TableId;
use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{
    Database, Datum, ExecOptions, REDACTED, execute as interpret, mask_last4,
};
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{
    Action, DatabaseEntities, Decision, Effect, EntityUid, Mask, Policy, PolicyProvider, Request,
    RlsProvider, SecValue, SecurityContext, parse_policies,
};
use interlaken_ir::text::parse_plan;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use security::{DOCS, USERS, catalog, sample};
use security_gen::{bag, condition, data, doc_atom, policies_for, query, user_atom, visible};
use support::session::{activate_rls, admin, create, install, replace_rows, rls_text, session};

/// A maskable column: resource type, table, attribute, column position, and masks that fit it.
type Maskable = (
    &'static str,
    TableId,
    &'static str,
    usize,
    &'static [&'static str],
);

/// The columns the generator masks.
const MASKABLE: [Maskable; 4] = [
    ("Doc", DOCS, "secret", 4, &["null"]),
    ("Doc", DOCS, "title", 3, &["null", "redact", "mask_last4"]),
    ("User", USERS, "level", 3, &["null"]),
    ("User", USERS, "name", 1, &["null", "redact", "mask_last4"]),
];

/// Generate the column policies of one maskable column, all with one mask, or none.
fn column_policies(column: Maskable) -> impl Strategy<Value = Vec<Policy>> {
    let (ty, _, attr, _, masks) = column;
    let atom = if ty == "Doc" { doc_atom() } else { user_atom() };
    let policy = (any::<bool>(), condition(atom));
    proptest::option::of((proptest::collection::vec(policy, 1..3), 0..masks.len())).prop_map(
        move |chosen| {
            let Some((policies, mask)) = chosen else {
                return Vec::new();
            };
            policies
                .into_iter()
                .enumerate()
                .map(|(i, (permit, condition))| Policy {
                    // The first is a permit, so some rows can show the value.
                    effect: if permit || i == 0 {
                        Effect::Permit
                    } else {
                        Effect::Forbid
                    },
                    actions: vec![Action::Read],
                    resource_type: ty.into(),
                    condition,
                    column: Some(attr.into()),
                    mask: Some(Mask::named(masks[mask])),
                })
                .collect()
        },
    )
}

/// Apply `mask` to a value.
fn apply_mask(mask: &Mask, value: &Datum) -> Datum {
    match (mask, value) {
        (Mask::Null, _) | (_, Datum::Null) => Datum::Null,
        (Mask::Function(f), Datum::Text(t)) if f.as_str() == "redact" => {
            let _ = t;
            Datum::Text(REDACTED.into())
        }
        (Mask::Function(f), Datum::Text(t)) if f.as_str() == "mask_last4" => {
            Datum::Text(mask_last4(t))
        }
        other => panic!("unexpected mask {other:?}"),
    }
}

/// Return `shown` (the visible rows of `db`) with every masked column
/// replaced on the rows its column policies deny, decided concretely.
fn masked(
    db: &Database,
    shown: &Database,
    catalog: &InMemoryCatalog,
    policies: &[Policy],
    context: &SecurityContext,
) -> Database {
    let store = DatabaseEntities { catalog, db };
    let mut out = shown.clone();
    for table in [DOCS, USERS] {
        let rows: Vec<Vec<Datum>> = shown
            .rows(table)
            .iter()
            .map(|row| {
                let resource = store.resource(table, row).expect("governed table");
                let mut new = row.clone();
                for (ty, t, attr, position, _) in MASKABLE {
                    let governing: Vec<Policy> = policies
                        .iter()
                        .filter(|p| {
                            t == table
                                && p.column.as_ref().map(interlaken_ir::Ident::as_str) == Some(attr)
                        })
                        .map(|p| Policy {
                            column: None,
                            mask: None,
                            ..p.clone()
                        })
                        .collect();
                    let Some(mask) = policies
                        .iter()
                        .find(|p| {
                            p.resource_type.as_str() == ty
                                && p.column.as_ref().map(interlaken_ir::Ident::as_str) == Some(attr)
                        })
                        .and_then(|p| p.mask.clone())
                    else {
                        continue;
                    };
                    if governing.is_empty() {
                        continue;
                    }
                    let provider = RlsProvider::new(governing);
                    let req = Request {
                        context,
                        action: Action::Read,
                        resource: &resource,
                    };
                    if provider.authorize(&req, &store).unwrap() == Decision::Deny {
                        new[position] = apply_mask(&mask, &row[position]);
                    }
                }
                new
            })
            .collect();
        out.set_rows(table, rows);
    }
    out
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

/// Construct the runtime and an engine with the fixture's catalog.
fn fixture() -> (tokio::runtime::Runtime, Engine, tempfile::TempDir) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = runtime.block_on(async {
        let engine = create(dir.path()).await;
        install(&mut session(&engine, admin()), &catalog()).await;
        engine
    });
    (runtime, engine, dir)
}

#[test]
/// Assert masked columns show exactly the concretely masked values, row by row.
fn masks_differential() {
    let (runtime, engine, _dir) = fixture();
    let catalog = catalog();
    let cases = 500;
    let counters: [AtomicU32; 3] = Default::default();
    let [compared, with_masks, changed] = &counters;
    let strategy = (
        data(),
        policies_for("Doc", doc_atom()),
        policies_for("User", user_atom()),
        (
            column_policies(MASKABLE[0]),
            column_policies(MASKABLE[1]),
            column_policies(MASKABLE[2]),
            column_policies(MASKABLE[3]),
        ),
        1i64..7,
        prop_oneof![
            query(),
            Just("(plan (project (get docs :as d) d.id d.title d.secret))".to_owned()),
            Just("(plan (project (get users :as u) u.id u.name u.level))".to_owned()),
        ],
    );
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(
        &strategy,
        |(data, mut policies, user_policies, (a, b, c, d), principal, query)| {
            policies.extend(user_policies);
            let any_masks = !(a.is_empty() && b.is_empty() && c.is_empty() && d.is_empty());
            for column in [a, b, c, d] {
                policies.extend(column);
            }
            let db = data.database();
            let provider = RlsProvider::new(policies.clone());
            let mut context = SecurityContext::new(EntityUid::long("User", principal));
            context.application = vec![("tenant".into(), SecValue::Str("t1".into()))];
            let plan = resolve(&parse_plan(&query).unwrap(), &catalog)
                .unwrap()
                .plan;
            let shown = visible(&db, &catalog, &provider, &context);
            let masked_db = masked(&db, &shown, &catalog, &policies, &context);
            let Ok(expected) = interpret(
                &plan,
                &catalog,
                &mut masked_db.clone(),
                &[],
                ExecOptions::default(),
            ) else {
                return Ok(());
            };
            let text = rls_text(&policies);
            let actual = runtime.block_on(async {
                store(&engine, &catalog, &db).await;
                activate_rls(&mut session(&engine, admin()), &text)
                    .await
                    .unwrap_or_else(|e| panic!("{e}: {text}"));
                engine
                    .session(context.clone())
                    .query(&query, Vec::new())
                    .await
                    .map(|r| r.rows())
            });
            let describe =
                || format!("principal {principal}\npolicies {text}\nquery {query}\ndata {data:?}");
            let actual = actual.map_err(|e| {
                TestCaseError::fail(format!(
                    "the session failed ({e}) where the masked rows succeed\n{}",
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
            if any_masks {
                with_masks.fetch_add(1, Ordering::Relaxed);
                let unmasked = interpret(
                    &plan,
                    &catalog,
                    &mut shown.clone(),
                    &[],
                    ExecOptions::default(),
                );
                if unmasked.is_ok_and(|u| bag(u) != bag(expected)) {
                    changed.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        },
    )
    .unwrap();
    let [compared, with_masks, changed] = counters.map(|c| c.load(Ordering::Relaxed));
    eprintln!("compared {compared} of {cases}, {with_masks} with masks, {changed} changed by them");
    assert!(
        compared * 10 >= cases * 8,
        "only {compared} of {cases} cases compared"
    );
    assert!(changed * 20 >= with_masks, "masks rarely changed a result");
}

#[test]
/// Assert `EXPLAIN` hides masks from principals without `read-policies`,
/// and masked columns can't be written through or read back by a write.
fn masks_in_explain_and_writes() {
    let (runtime, engine, _dir) = fixture();
    let catalog = catalog();
    runtime.block_on(async {
        store(&engine, &catalog, &sample().database()).await;
        let policies = "(policies
            (permit (read update insert) Doc true)
            (permit (read) User true)
            (permit (read) Doc (== resource.owner principal) :column title :mask mask_last4)
            (permit (read) Doc (== resource.owner principal) :column secret :mask null))";
        activate_rls(&mut session(&engine, admin()), policies)
            .await
            .unwrap();
        let mut ann = session(&engine, EntityUid::long("User", 1));
        let rows = ann
            .query(
                "(plan (sort (project (get docs :as d) d.id d.title d.secret) (asc id)))",
                Vec::new(),
            )
            .await
            .unwrap()
            .rows();
        let t = |s: &str| Datum::Text(s.into());
        assert_eq!(
            rows,
            [
                vec![Datum::Int8(10), t("a plan"), Datum::Int8(5)],
                vec![Datum::Int8(11), t("bad\\"), Datum::Null],
                vec![Datum::Int8(12), t("******mber"), Datum::Null],
                vec![Datum::Int8(13), t("x"), Datum::Null],
            ]
        );
        // Filters see masked values: hidden secrets are NULL.
        let filtered = ann
            .query("(plan (project (filter (get docs :as d) (> d.secret 1)) d.id))", Vec::new())
            .await
            .unwrap()
            .rows();
        assert_eq!(filtered, [vec![Datum::Int8(10)]]);
        let query = "(plan (project (get docs :as d) d.id d.title))";
        let hidden = ann.explain(query, false).await.unwrap();
        assert!(hidden.contains("<policy> mask"), "{hidden}");
        assert!(!hidden.contains("(call"), "{hidden}");
        // Administrators see the mask's call (the condition is false for them: they own nothing).
        let shown = session(&engine, admin()).explain(query, false).await.unwrap();
        assert!(shown.contains("(call (fid") && !shown.contains("<policy>"), "{shown}");
        // A write reading a masked column fails rather than computing from hidden values.
        let err = ann
            .query(
                "(plan (update docs (get docs :as d) (set title (call upper d.title)) :as d))",
                Vec::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate(), "42501", "{err}");
        // So does writing it.
        let err = ann
            .query(
                "(plan (insert docs (values (columns id owner_id title) (row 20 2 \"new\")) (columns id owner_id title)))",
                Vec::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate(), "42501", "{err}");
        // Any write to the table carries whole target rows, so it fails too
        // (conservatively, as for a denied column).
        let err = ann
            .query(
                "(plan (delete docs (filter (get docs :as d) (= d.id 13)) :as d))",
                Vec::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate(), "42501", "{err}");
        // Conflicting masks are an error, not a guess.
        let conflicting = parse_policies(
            "(policies (permit (read) Doc true)
               (permit (read) Doc (== resource.owner principal) :column title :mask null)
               (permit (read) Doc (< resource.secret 3) :column title :mask redact))",
        )
        .unwrap();
        activate_rls(&mut session(&engine, admin()), &rls_text(&conflicting))
            .await
            .unwrap();
        let err = ann.query(query, Vec::new()).await.unwrap_err();
        assert_eq!(err.sqlstate(), "42501", "{err}");
        assert!(err.to_string().contains("conflicting masks"), "{err}");
    });
}
