//! Random data, queries and authorization for the security oracles.

#![allow(dead_code)]

use std::collections::HashMap;

use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Database, Datum, RowKey};
use interlaken_ir::security::{
    Action, DatabaseEntities, Decision, PolicyProvider, Request, SecurityContext,
};
use proptest::prelude::*;

use super::security::{DOCS, Data, FOLDERS, GROUPS, USERS};

/// Generate random entity data.
pub fn data() -> impl Strategy<Value = Data> {
    let dept = proptest::option::of(prop_oneof![Just("eng".to_owned()), Just("ops".to_owned())]);
    let user = (dept, proptest::option::of(0i64..5));
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
                    .map(|(i, (dept, level))| (i as i64 + 1, format!("u{}", i + 1), dept, level))
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
