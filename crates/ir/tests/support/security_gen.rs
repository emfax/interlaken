//! Random data, queries and authorization for the security oracles.

#![allow(dead_code)]

use std::collections::HashMap;

use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Database, Datum, RowKey};
use interlaken_ir::security::{
    Action, ArithOp, CmpOp, DatabaseEntities, Decision, Effect, EntityUid, Policy, PolicyProvider,
    Request, SecExpr, SecValue, SecurityContext,
};
use proptest::prelude::*;

use super::security::{DOCS, Data, FOLDERS, GROUPS, USERS};

/// Generate random entity data.
pub fn data() -> impl Strategy<Value = Data> {
    let dept = proptest::option::of(prop_oneof![Just("eng".to_owned()), Just("ops".to_owned())]);
    // Managers range over one more ID than there are users at most, so some are missing.
    let user = (
        dept,
        proptest::option::of(0i64..5),
        proptest::option::weighted(0.8, 1i64..6),
    );
    let doc = (
        1i64..6,
        proptest::option::of(1i64..5),
        prop_oneof![Just("a plan"), Just("x"), Just("ba"), Just("bad\\")].prop_map(str::to_owned),
        proptest::option::of(0i64..12),
        proptest::option::of(proptest::collection::vec(
            prop_oneof![Just("a".to_owned()), Just("b".to_owned())],
            0..3,
        )),
    );
    (
        proptest::collection::vec(user, 1..5),
        proptest::collection::vec(proptest::option::of(0i64..10), 4),
        proptest::collection::vec(doc, 0..6),
        proptest::collection::vec((1i64..6, 1i64..4), 0..5),
    )
        .prop_map(|(users, parents, docs, memberships)| {
            let n = users.len() as i64;
            Data {
                users: users
                    .into_iter()
                    .enumerate()
                    .map(|(i, (dept, level, manager))| {
                        (i as i64 + 1, format!("u{}", i + 1), dept, level, manager)
                    })
                    .collect(),
                groups: vec![1, 2, 3],
                // Folder f's parent, if any, has a smaller ID, so the hierarchy is acyclic.
                folders: parents
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let f = i as i64 + 1;
                        (f, p.filter(|p| *p >= 1 && *p < f))
                    })
                    .collect(),
                docs: docs
                    .into_iter()
                    .enumerate()
                    .map(|(i, (owner, folder, title, secret, tags))| {
                        (i as i64 + 10, owner.min(n), folder, title, secret, tags)
                    })
                    .collect(),
                user_groups: {
                    let mut m: Vec<(i64, i64)> =
                        memberships.into_iter().filter(|(u, _)| *u <= n).collect();
                    m.sort_unstable();
                    m.dedup();
                    m
                },
            }
        })
}

/// Generate a query over documents and users.
pub fn query() -> impl Strategy<Value = String> {
    let k = 0i64..12;
    prop_oneof![
        Just("(plan (project (get docs :as d) d.id d.title))".to_owned()),
        k.clone().prop_map(|k| format!("(plan (project (filter (get docs :as d) (> d.secret {k})) d.id))")),
        Just("(plan (aggregate (get docs :as d) (keys d.owner_id) (aggs (agg count) (agg max d.secret))))".to_owned()),
        Just("(plan (project (join inner (get docs :as d) (get users :as u) (= d.owner_id u.id)) d.id u.name))".to_owned()),
        Just("(plan (project (get users :as u) u.name (as (scalar (aggregate (filter (get docs :as d) (= d.owner_id u.id)) (keys) (aggs (agg count)))) n)))".to_owned()),
        Just("(plan (project (filter (get users :as u) (exists (filter (get docs :as d) (= d.owner_id u.id)))) u.id))".to_owned()),
        Just("(plan (limit (sort (project (get docs :as d) d.id d.secret) (desc secret) (asc id)) :count 2))".to_owned()),
        Just("(plan (project (apply left (get users :as u) (nest (sort (filter (get docs :as d) (= d.owner_id u.id)) (asc d.id)) (keys) (fields d.id) :as docs)) u.id docs))".to_owned()),
        Just("(plan (project (filter (get docs :as d) (like d.title \"a%\")) d.id))".to_owned()),
        Just("(plan (aggregate (get users :as u) (keys) (aggs (agg count) (agg sum u.level))))".to_owned()),
        k.clone().prop_map(|k| format!("(plan (project (filter (get docs :as d) (= (/ 100 (- d.secret {k})) 1)) d.id))")),
        k.prop_map(|k| format!("(plan (project (filter (get docs :as d) (not (in d.owner_id (project (filter (get users :as u) (< u.level {k})) u.id)))) d.id))")),
    ]
}

/// Return the rows of `db` the principal may read, by concrete authorization of each row.
pub fn visible(
    db: &Database,
    catalog: &InMemoryCatalog,
    provider: &dyn PolicyProvider,
    context: &SecurityContext,
) -> Database {
    let store = DatabaseEntities { catalog, db };
    let mut out = db.clone();
    for table in [USERS, GROUPS, FOLDERS, DOCS] {
        let rows = db
            .rows(table)
            .iter()
            .filter(|row| {
                let resource = store.resource(table, row).expect("governed table");
                let req = Request {
                    context,
                    action: Action::Read,
                    resource: &resource,
                };
                provider.authorize(&req, &store).expect("authorize") == Decision::Allow
            })
            .cloned()
            .collect();
        out.set_rows(table, rows);
    }
    out
}

/// Count rows by value.
pub fn bag(rows: Vec<Vec<Datum>>) -> HashMap<RowKey, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(RowKey(row)).or_insert(0) += 1;
    }
    counts
}

/// Box an expression.
pub fn bx(e: SecExpr) -> Box<SecExpr> {
    Box::new(e)
}

/// Return `resource.a.b...`.
pub fn path(base: SecExpr, attrs: &[&str]) -> SecExpr {
    attrs
        .iter()
        .fold(base, |e, a| SecExpr::Attr(bx(e), (*a).into()))
}

/// Return an integer literal.
pub fn long(n: i64) -> SecExpr {
    SecExpr::Lit(SecValue::Long(n))
}

/// Return a string literal.
pub fn string(s: &str) -> SecExpr {
    SecExpr::Lit(SecValue::Str(s.into()))
}

/// Return an entity literal.
pub fn entity(ty: &str, key: i64) -> SecExpr {
    SecExpr::Lit(SecValue::Entity(EntityUid::long(ty, key)))
}

/// Construct a comparison.
pub fn cmp(op: CmpOp, a: SecExpr, b: SecExpr) -> SecExpr {
    SecExpr::Cmp(op, bx(a), bx(b))
}

/// Generate atomic conditions over documents.
pub fn doc_atom() -> BoxedStrategy<SecExpr> {
    use SecExpr::{Principal as P, Resource as R};
    prop_oneof![
        Just(cmp(CmpOp::Eq, path(R, &["owner"]), P)),
        (1i64..4).prop_map(|g| SecExpr::In(bx(P), bx(entity("Group", g)))),
        (1i64..5).prop_map(|f| SecExpr::In(bx(R), bx(entity("Folder", f)))),
        (0i64..12).prop_map(|k| cmp(CmpOp::Lt, path(R, &["secret"]), long(k))),
        prop_oneof![Just("a*"), Just("*a*"), Just("x"), Just("*")]
            .prop_map(|g| SecExpr::Like(bx(path(R, &["title"])), g.into())),
        prop_oneof![Just("eng"), Just("ops")].prop_map(|d| cmp(
            CmpOp::Eq,
            path(P, &["dept"]),
            string(d)
        )),
        Just(SecExpr::Has(bx(R), "secret".into())),
        prop_oneof![Just("a"), Just("b")]
            .prop_map(|t| SecExpr::Contains(bx(path(R, &["tags"])), bx(string(t)))),
        Just(cmp(
            CmpOp::Eq,
            path(R, &["owner", "dept"]),
            path(P, &["dept"])
        )),
        // Entities beyond the principal (ADR-0032).
        Just(cmp(CmpOp::Eq, path(R, &["owner"]), path(P, &["manager"]))),
        Just(cmp(
            CmpOp::Eq,
            path(R, &["owner", "dept"]),
            path(P, &["manager", "dept"])
        )),
        (0i64..5).prop_map(|k| cmp(
            CmpOp::Lt,
            path(P, &["manager", "manager", "level"]),
            long(k)
        )),
        (1i64..4).prop_map(|g| SecExpr::In(bx(path(P, &["manager"])), bx(entity("Group", g)))),
        Just(cmp(
            CmpOp::Eq,
            path(SecExpr::Context, &["app", "tenant"]),
            string("t1")
        )),
        (0i64..12).prop_map(|k| cmp(
            CmpOp::Gt,
            SecExpr::Arith(ArithOp::Add, bx(path(R, &["secret"])), bx(long(1))),
            long(k)
        )),
        Just(SecExpr::In(bx(P), bx(path(R, &["owner"])))),
        prop_oneof![Just("f1"), Just("f*")]
            .prop_map(|g| SecExpr::Like(bx(path(R, &["folder", "name"])), g.into())),
        (0i64..5).prop_map(|k| cmp(CmpOp::Lt, path(P, &["level"]), long(k))),
        Just(SecExpr::Lit(SecValue::Bool(true))),
    ]
    .boxed()
}

/// Generate atomic conditions over users.
pub fn user_atom() -> BoxedStrategy<SecExpr> {
    use SecExpr::{Principal as P, Resource as R};
    prop_oneof![
        Just(cmp(CmpOp::Eq, R, P)),
        Just(cmp(CmpOp::Eq, path(R, &["dept"]), path(P, &["dept"]))),
        (1i64..4).prop_map(|g| SecExpr::In(bx(R), bx(entity("Group", g)))),
        (0i64..5).prop_map(|k| cmp(CmpOp::Lt, path(R, &["level"]), long(k))),
        Just(SecExpr::Has(bx(R), "dept".into())),
        Just(cmp(CmpOp::Eq, path(R, &["manager"]), P)),
        Just(cmp(
            CmpOp::Eq,
            path(R, &["dept"]),
            path(P, &["manager", "dept"])
        )),
        Just(SecExpr::Has(bx(path(P, &["manager"])), "level".into())),
        Just(SecExpr::Lit(SecValue::Bool(true))),
    ]
    .boxed()
}

/// Generate conditions combining atoms with `&&`, `||`, `!` and `if`.
pub fn condition(atom: BoxedStrategy<SecExpr>) -> BoxedStrategy<SecExpr> {
    atom.prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| SecExpr::And(bx(a), bx(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| SecExpr::Or(bx(a), bx(b))),
            inner.clone().prop_map(|a| SecExpr::Not(bx(a))),
            (inner.clone(), inner.clone(), inner).prop_map(|(c, a, b)| SecExpr::If(
                bx(c),
                bx(a),
                bx(b)
            )),
        ]
    })
    .boxed()
}

/// Generate the policies for one resource type.
pub fn policies_for(
    resource_type: &'static str,
    atom: BoxedStrategy<SecExpr>,
) -> impl Strategy<Value = Vec<Policy>> {
    let policy = move |effect| {
        condition(atom.clone()).prop_map(move |condition| Policy {
            effect,
            actions: vec![Action::Read],
            resource_type: resource_type.into(),
            condition,
            column: None,
            mask: None,
        })
    };
    (
        proptest::collection::vec(policy(Effect::Permit), 0..4),
        proptest::collection::vec(policy(Effect::Forbid), 0..3),
    )
        .prop_map(|(mut permits, forbids)| {
            permits.extend(forbids);
            permits
        })
}
