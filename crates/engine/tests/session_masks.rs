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
use support::session::{
    activate_rls, admin, create, install, replace_rows, rls_text, session, stored_rows,
};

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
    // `title` is plain text: only masks of its own type (ADR-0034).
    ("Doc", DOCS, "title", 3, &["redact", "mask_last4"]),
    ("User", USERS, "level", 3, &["null"]),
    // `name` is plain text: only masks of its own type (ADR-0034).
    ("User", USERS, "name", 1, &["redact", "mask_last4"]),
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

/// Return `row` of `table` with every masked column replaced where its
/// column policies, decided concretely for the row over `db`, deny it.
fn mask_row(
    db: &Database,
    catalog: &InMemoryCatalog,
    policies: &[Policy],
    context: &SecurityContext,
    table: TableId,
    row: &[Datum],
) -> Vec<Datum> {
    let store = DatabaseEntities { catalog, db };
    let resource = store.resource(table, row).expect("governed table");
    let mut new = row.to_vec();
    for (ty, t, attr, position, _) in MASKABLE {
        let governing: Vec<Policy> = policies
            .iter()
            .filter(|p| {
                t == table && p.column.as_ref().map(interlaken_ir::Ident::as_str) == Some(attr)
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
    let mut out = shown.clone();
    for table in [DOCS, USERS] {
        let rows: Vec<Vec<Datum>> = shown
            .rows(table)
            .iter()
            .map(|row| mask_row(db, catalog, policies, context, table, row))
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
/// and conflicting masks are an error.
fn masks_in_explain() {
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
        // So do writes' masks, on the target row and in `RETURNING`.
        let write = "(plan (update docs (filter (get docs :as d) (> d.secret 1)) (set folder_id 1) (returning t.title) :as t))";
        let hidden = ann.explain(write, false).await.unwrap();
        assert!(hidden.contains("<policy> mask"), "{hidden}");
        assert!(!hidden.contains("(call"), "{hidden}");
        // Administrators see the mask's call (the condition is false for them: they own nothing).
        let shown = session(&engine, admin()).explain(query, false).await.unwrap();
        assert!(shown.contains("(call (fid") && !shown.contains("<policy>"), "{shown}");
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

/// The policies of the masked-write tests: everyone may read and write
/// every document and read every user, but only an owner sees a
/// document's title (else `mask_last4`) and secret (else NULL).
const WRITE_POLICIES: &str = "(policies
    (permit (read update insert delete) Doc true)
    (permit (read) User true)
    (permit (read) Doc (== resource.owner principal) :column title :mask mask_last4)
    (permit (read) Doc (== resource.owner principal) :column secret :mask null))";

/// Construct a text datum.
fn text(s: &str) -> Datum {
    Datum::Text(s.into())
}

/// Return the stored `(id, owner_id, folder_id, title, secret)` of every document.
async fn stored_docs(engine: &Engine) -> Vec<Vec<Datum>> {
    stored_rows(engine, "docs")
        .await
        .into_iter()
        .map(|mut row| {
            row.truncate(5);
            row
        })
        .collect()
}

/// The sample documents as stored: `(id, owner_id, folder_id, title, secret)`.
fn sample_docs() -> Vec<Vec<Datum>> {
    vec![
        vec![
            Datum::Int8(10),
            Datum::Int8(1),
            Datum::Int8(2),
            text("a plan"),
            Datum::Int8(5),
        ],
        vec![
            Datum::Int8(11),
            Datum::Int8(2),
            Datum::Int8(3),
            text("bad\\"),
            Datum::Int8(42),
        ],
        vec![
            Datum::Int8(12),
            Datum::Int8(2),
            Datum::Null,
            text("notanumber"),
            Datum::Int8(7),
        ],
        vec![
            Datum::Int8(13),
            Datum::Int8(3),
            Datum::Int8(1),
            text("x"),
            Datum::Null,
        ],
    ]
}

/// Construct the fixture with the sample data and [`WRITE_POLICIES`] active.
fn write_fixture() -> (tokio::runtime::Runtime, Engine, tempfile::TempDir) {
    let (runtime, engine, dir) = fixture();
    runtime.block_on(async {
        store(&engine, &catalog(), &sample().database()).await;
        activate_rls(&mut session(&engine, admin()), WRITE_POLICIES)
            .await
            .unwrap();
    });
    (runtime, engine, dir)
}

/// Run `statement` as ann (User 1, who owns only document 10) and return
/// its rows; then return the stored documents and restore the sample.
async fn write_as_ann(engine: &Engine, statement: &str) -> (Vec<Vec<Datum>>, Vec<Vec<Datum>>) {
    let rows = session(engine, EntityUid::long("User", 1))
        .query(statement, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{statement}: {e}"))
        .rows();
    let stored = stored_docs(engine).await;
    store(engine, &catalog(), &sample().database()).await;
    (rows, stored)
}

#[test]
/// Assert writes to a table with masked columns succeed, carry the
/// masked columns' real values through, and read them only masked.
fn masked_writes() {
    let (runtime, engine, _dir) = write_fixture();
    runtime.block_on(async {
        let i = Datum::Int8;
        // An update that doesn't touch the masked columns keeps their values.
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (update docs (filter (get docs :as d) (= d.id 12)) (set folder_id 3) :as d))",
        )
        .await;
        let mut expected = sample_docs();
        expected[2][2] = i(3);
        assert_eq!(stored, expected);
        // So does one over every row.
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (update docs (get docs :as d) (set folder_id (+ d.id 1)) :as d))",
        )
        .await;
        let mut expected = sample_docs();
        for row in &mut expected {
            let Datum::Int8(id) = row[0] else { panic!() };
            row[2] = i(id + 1);
        }
        assert_eq!(stored, expected);
        // Predicates see masked values: ann sees only her own secret...
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (update docs (filter (get docs :as d) (= d.secret 7)) (set folder_id 1) :as d))",
        )
        .await;
        assert_eq!(stored, sample_docs());
        // ... and hidden secrets are NULL, but stay intact when carried.
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (update docs (filter (get docs :as d) (is-null d.secret)) (set folder_id 1) :as d))",
        )
        .await;
        let mut expected = sample_docs();
        for row in &mut expected[1..] {
            row[2] = i(1);
        }
        assert_eq!(stored, expected);
        // Assignments read masked values, so copying a masked column into
        // another copies the masked value, never the hidden one.
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (update docs (get docs :as d) (set folder_id d.secret) :as d))",
        )
        .await;
        let mut expected = sample_docs();
        expected[0][2] = i(5);
        for row in &mut expected[1..] {
            row[2] = Datum::Null;
        }
        assert_eq!(stored, expected);
        // Assigning a masked column to itself writes what the principal
        // sees: the update's assignments are its own, like literals.
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (update docs (filter (get docs :as d) (>= d.id 12)) (set title (call upper d.title)) :as d))",
        )
        .await;
        let mut expected = sample_docs();
        expected[2][3] = text("******MBER");
        expected[3][3] = text("X");
        assert_eq!(stored, expected);
        // A join condition on a masked column sees the masked value too.
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (update docs (join inner (get docs :as d) (get users :as u) (= d.secret u.id)) (set folder_id u.id) :as d))",
        )
        .await;
        assert_eq!(stored, sample_docs(), "no visible secret is a user ID");
        // `RETURNING` masks the written row as a later read would.
        let (rows, stored) = write_as_ann(
            &engine,
            "(plan (update docs (filter (get docs :as d) (>= d.id 11)) (set folder_id 2) (returning t.id t.title t.secret (as (+ t.secret 1) next)) :as t))",
        )
        .await;
        assert_eq!(
            rows,
            [
                vec![i(11), text("bad\\"), Datum::Null, Datum::Null],
                vec![i(12), text("******mber"), Datum::Null, Datum::Null],
                vec![i(13), text("x"), Datum::Null, Datum::Null],
            ]
        );
        let mut expected = sample_docs();
        for row in &mut expected[1..] {
            row[2] = i(2);
        }
        assert_eq!(stored, expected);
        // Decided for the new row: giving a document away masks it.
        let (rows, _) = write_as_ann(
            &engine,
            "(plan (update docs (filter (get docs :as d) (= d.id 10)) (set owner_id 2) (returning t.title t.secret) :as t))",
        )
        .await;
        assert_eq!(rows, [vec![text("**plan"), Datum::Null]]);
        // Deletes filter on masked values.
        let (rows, stored) = write_as_ann(
            &engine,
            "(plan (delete docs (filter (get docs :as d) (= d.secret 42)) (returning t.id) :as t))",
        )
        .await;
        assert!(rows.is_empty());
        assert_eq!(stored, sample_docs());
        let (rows, stored) = write_as_ann(
            &engine,
            "(plan (delete docs (filter (get docs :as d) (or (= d.secret 5) (= d.title \"******mber\"))) (returning t.id t.title t.secret) :as t))",
        )
        .await;
        assert_eq!(
            rows,
            [
                vec![i(10), text("a plan"), i(5)],
                vec![i(12), text("******mber"), Datum::Null],
            ]
        );
        assert_eq!(stored, [sample_docs()[1].clone(), sample_docs()[3].clone()]);
        // A delete that reads no masked column carries none.
        let (rows, stored) = write_as_ann(
            &engine,
            "(plan (delete docs (filter (get docs :as d) (= d.owner_id 2)) (returning t.id) :as t))",
        )
        .await;
        assert_eq!(rows, [vec![i(11)], vec![i(12)]]);
        assert_eq!(stored, [sample_docs()[0].clone(), sample_docs()[3].clone()]);
        // Inserts write the values given, and return them masked.
        let (rows, stored) = write_as_ann(
            &engine,
            "(plan (insert docs (values (columns id owner_id title secret) (row 20 2 \"new title\" 9)) (columns id owner_id title secret) (returning t.title t.secret) :as t))",
        )
        .await;
        assert_eq!(rows, [vec![text("*****itle"), Datum::Null]]);
        assert_eq!(
            stored[4],
            [i(20), i(2), Datum::Null, text("new title"), i(9)]
        );
        // Copying rows copies what the principal reads: masked values.
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (insert docs (project (filter (get docs :as d) (= d.id 12)) (+ d.id 100) d.owner_id d.title d.secret) (columns id owner_id title secret)))",
        )
        .await;
        assert_eq!(
            stored[4],
            [i(112), i(2), Datum::Null, text("******mber"), Datum::Null]
        );
        // Policies check the written row's real values, never masked ones:
        // updates need a secret below 100 before (USING) and after (WITH CHECK).
        activate_rls(
            &mut session(&engine, admin()),
            "(policies (permit (read insert delete) Doc true)
               (permit (update) Doc (< resource.secret 100))
               (permit (read) User true)
               (permit (read) Doc (== resource.owner principal) :column secret :mask null))",
        )
        .await
        .unwrap();
        let (_, stored) = write_as_ann(
            &engine,
            "(plan (update docs (filter (get docs :as d) (is-null d.secret)) (set folder_id 1) :as d))",
        )
        .await;
        let mut expected = sample_docs();
        expected[1][2] = i(1);
        expected[2][2] = i(1);
        assert_eq!(stored, expected, "document 13 has no secret, so USING excludes it");
        // A carried target row consumed by position can't stay unmasked.
        activate_rls(
            &mut session(&engine, admin()),
            "(policies (permit (read delete) User true)
               (permit (read) User (== resource principal) :column level :mask null))",
        )
        .await
        .unwrap();
        let users = stored_rows(&engine, "users").await;
        let mut ann = session(&engine, EntityUid::long("User", 1));
        let err = ann
            .query("(plan (delete users (distinct (get users :as u)) :as u))", Vec::new())
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate(), "42501", "{err}");
        // Without `DISTINCT` it carries the row.
        ann.query(
            "(plan (delete users (filter (get users :as u) (= u.level 3)) :as u))",
            Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(stored_rows(&engine, "users").await, users[1..]);
    });
}

/// A generated write to `docs`: an update (with an assignment) or a delete.
#[derive(Clone, Debug)]
struct Write {
    /// The input relation, reading the target as `d`.
    input: String,
    /// For an update: the assigned column's position and name, and its value over the input.
    assignment: Option<(usize, &'static str, &'static str)>,
    /// Whether the statement returns `id`, `title` and `secret`.
    returning: bool,
}

impl Write {
    /// Return the statement's text.
    fn statement(&self) -> String {
        let returning = if self.returning {
            " (returning t.id t.title t.secret)"
        } else {
            ""
        };
        match self.assignment {
            Some((_, column, value)) => format!(
                "(plan (update docs {} (set {column} {value}){returning} :as t))",
                self.input
            ),
            None => format!("(plan (delete docs {}{returning} :as t))", self.input),
        }
    }

    /// Return a query selecting each target row's ID and, for an update,
    /// its new value: run over the masked rows, it is what the write must do.
    fn targets(&self) -> String {
        match self.assignment {
            Some((_, _, value)) => format!("(plan (project {} d.id {value}))", self.input),
            None => format!("(plan (project {} d.id))", self.input),
        }
    }
}

/// Generate writes to `docs` whose predicates and assignments read masked columns.
fn write() -> impl Strategy<Value = Write> {
    let k = 0i64..12;
    let filter = prop_oneof![
        1 => k.clone().prop_map(|k| format!("(= d.secret {k})")),
        1 => Just("(is-null d.secret)".to_owned()),
        1 => Just("(like d.title \"a%\")".to_owned()),
        1 => Just("(like d.title \"*%\")".to_owned()),
        1 => k.prop_map(|k| format!("(> d.id {})", k + 9)),
        1 => (1i64..5).prop_map(|u| format!("(= d.owner_id {u})")),
        3 => Just("true".to_owned()),
    ];
    let input = prop_oneof![
        4 => filter.prop_map(|f| format!("(filter (get docs :as d) {f})")),
        1 => Just("(join inner (get docs :as d) (get users :as u) (= d.owner_id u.id))".to_owned()),
        1 => Just("(filter (join inner (get docs :as d) (get users :as u) (= d.owner_id u.id)) (like u.name \"*%\"))".to_owned()),
    ];
    let assignment = proptest::option::weighted(
        0.75,
        prop_oneof![
            Just((2, "folder_id", "d.secret")),
            Just((4, "secret", "(+ d.secret 1)")),
            Just((3, "title", "(call upper d.title)")),
            Just((3, "title", "d.title")),
            Just((2, "folder_id", "1")),
            Just((1, "owner_id", "1")),
            Just((4, "secret", "d.owner_id")),
            Just((4, "secret", "(coalesce d.secret 0)")),
        ],
    );
    (input, assignment, proptest::bool::weighted(0.7)).prop_map(|(input, assignment, returning)| {
        // Assignments from the joined user read `u`.
        let assignment = match (input.contains("users"), assignment) {
            (true, Some((3, ..))) => Some((3, "title", "u.name")),
            (_, a) => a,
        };
        Write {
            input,
            assignment,
            returning,
        }
    })
}

/// Return the integer in `datum`.
fn int(datum: &Datum) -> i64 {
    match datum {
        Datum::Int8(v) => *v,
        other => panic!("expected an int8, found {other:?}"),
    }
}

#[test]
/// Assert writes to masked tables read masked values (predicates, joins,
/// assignments), write every other column back unmasked, and return
/// masked rows, against a model run over the masked rows.
fn masked_writes_differential() {
    let (runtime, engine, _dir) = fixture();
    let catalog = catalog();
    let cases = 500;
    let counters: [AtomicU32; 3] = Default::default();
    let [compared, changed, masked_returns] = &counters;
    let writes = Policy {
        effect: Effect::Permit,
        actions: vec![Action::Update, Action::Delete],
        resource_type: "Doc".into(),
        condition: interlaken_ir::security::SecExpr::Lit(SecValue::Bool(true)),
        column: None,
        mask: None,
    };
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
        write(),
        proptest::bool::weighted(0.75),
    );
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(
        &strategy,
        |(data, mut policies, user_policies, (a, b, c, d), principal, write, readable)| {
            policies.extend(user_policies);
            for column in [a, b, c, d] {
                policies.extend(column);
            }
            policies.push(writes.clone());
            if readable {
                // Usually every document is readable, so writes find rows.
                policies.push(Policy {
                    actions: vec![Action::Read],
                    ..writes.clone()
                });
            }
            let db = data.database();
            let provider = RlsProvider::new(policies.clone());
            let mut context = SecurityContext::new(EntityUid::long("User", principal));
            context.application = vec![("tenant".into(), SecValue::Str("t1".into()))];
            let shown = visible(&db, &catalog, &provider, &context);
            let masked_db = masked(&db, &shown, &catalog, &policies, &context);
            let targets = resolve(&parse_plan(&write.targets()).unwrap(), &catalog)
                .unwrap()
                .plan;
            let Ok(targets) = interpret(
                &targets,
                &catalog,
                &mut masked_db.clone(),
                &[],
                ExecOptions::default(),
            ) else {
                return Ok(());
            };
            // The model: the targets' stored rows, changed by the values
            // computed over the masked rows, each row once.
            let mut stored: Vec<Vec<Datum>> = db.rows(DOCS).to_vec();
            let mut written = Vec::new();
            for target in &targets {
                let Some(i) = stored.iter().position(|r| r[0] == target[0]) else {
                    continue;
                };
                if written.iter().any(|(id, _)| *id == int(&target[0])) {
                    continue;
                }
                match write.assignment {
                    Some((column, ..)) => {
                        stored[i][column] = target[1].clone();
                        written.push((int(&target[0]), stored[i].clone()));
                    }
                    None => written.push((int(&target[0]), stored.remove(i))),
                }
            }
            let mut after = db.clone();
            after.set_rows(DOCS, stored.clone());
            let returned: Vec<Vec<Datum>> = written
                .iter()
                .map(|(_, row)| {
                    let source = if write.assignment.is_some() {
                        &after
                    } else {
                        &db
                    };
                    let shown = mask_row(source, &catalog, &policies, &context, DOCS, row);
                    vec![shown[0].clone(), shown[3].clone(), shown[4].clone()]
                })
                .collect();
            let statement = write.statement();
            let text = rls_text(&policies);
            let (actual, actual_stored) = runtime.block_on(async {
                store(&engine, &catalog, &db).await;
                activate_rls(&mut session(&engine, admin()), &text)
                    .await
                    .unwrap_or_else(|e| panic!("{e}: {text}"));
                let result = engine
                    .session(context.clone())
                    .query(&statement, Vec::new())
                    .await
                    .map(|r| r.rows());
                (result, stored_rows(&engine, "docs").await)
            });
            let describe = || {
                format!(
                    "principal {principal}\npolicies {text}\nstatement {statement}\ndata {data:?}"
                )
            };
            if !written.is_empty() {
                changed.fetch_add(1, Ordering::Relaxed);
            }
            // A masked NULL assigned to a NOT NULL column (`owner_id`, `title`) fails the write.
            let violates = written
                .iter()
                .any(|(_, r)| r[1] == Datum::Null || r[3] == Datum::Null);
            if violates {
                let mut original = db.rows(DOCS).to_vec();
                original.sort_by_key(|r| int(&r[0]));
                prop_assert!(
                    actual.as_ref().is_err_and(|e| e.sqlstate() == "23502")
                        && actual_stored == original,
                    "expected a not-null violation and no change, got {:?}\n{}",
                    actual.map(|_| actual_stored),
                    describe()
                );
                compared.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            let actual = actual.map_err(|e| {
                TestCaseError::fail(format!(
                    "the session failed ({e}) where the model succeeds\n{}",
                    describe()
                ))
            })?;
            let mut expected_stored = stored;
            expected_stored.sort_by_key(|r| int(&r[0]));
            prop_assert!(
                actual_stored == expected_stored,
                "stored rows differ\nexpected {:?}\nactual {:?}\n{}",
                expected_stored,
                actual_stored,
                describe()
            );
            if write.returning {
                prop_assert!(
                    bag(returned.clone()) == bag(actual.clone()),
                    "RETURNING differs\nexpected {:?}\nactual {:?}\n{}",
                    returned,
                    actual,
                    describe()
                );
                let unmasked: Vec<Vec<Datum>> = written
                    .iter()
                    .map(|(_, r)| vec![r[0].clone(), r[3].clone(), r[4].clone()])
                    .collect();
                if bag(unmasked) != bag(returned) {
                    masked_returns.fetch_add(1, Ordering::Relaxed);
                }
            }
            compared.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    )
    .unwrap();
    let [compared, changed, masked_returns] = counters.map(|c| c.load(Ordering::Relaxed));
    eprintln!(
        "compared {compared} of {cases}, {changed} writing rows, {masked_returns} returning masked values"
    );
    assert!(
        compared * 10 >= cases * 8,
        "only {compared} of {cases} cases compared"
    );
    assert!(changed * 5 >= compared, "writes rarely changed rows");
    assert!(masked_returns * 40 >= compared, "RETURNING rarely masked");
}

#[test]
/// Assert a NULL mask is refused on a column whose type isn't optional
/// (ADR-0034): it would make the column's type depend on who reads it.
/// Masks of the column's type are accepted, and the column stays plain.
fn null_masks_need_optional_columns() {
    let (runtime, engine, _dir) = fixture();
    runtime.block_on(async {
        store(&engine, &catalog(), &sample().database()).await;
        let mut root = session(&engine, admin());
        let query = "(plan (project (get users :as u) u.id u.name))";
        activate_rls(
            &mut root,
            "(policies (permit (read) User true)
               (permit (read) User (== resource principal) :column name :mask null))",
        )
        .await
        .unwrap();
        let err = session(&engine, EntityUid::long("User", 1))
            .query(query, Vec::new())
            .await
            .unwrap_err();
        assert_eq!(err.sqlstate(), "42501", "{err}");
        assert!(
            err.to_string()
                .contains("a NULL mask on column name, whose type isn't optional"),
            "{err}"
        );
        activate_rls(
            &mut root,
            "(policies (permit (read) User true)
               (permit (read) User (== resource principal) :column name :mask redact))",
        )
        .await
        .unwrap();
        let mut ann = session(&engine, EntityUid::long("User", 1));
        let plan = ann.explain(query, false).await.unwrap();
        let result = ann.query(query, Vec::new()).await.unwrap();
        assert!(!result.types[1].optional, "{plan}");
        assert!(
            result.rows().iter().any(|r| r[1] == text("****")),
            "{:?}",
            result.rows()
        );
    });
}
