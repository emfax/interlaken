//! The security fixture: catalog, sample data and closure maintenance.

#![allow(dead_code)]

use std::path::Path;

use interlaken_ir::TableId;
use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Database, Datum};
use interlaken_ir::security::{Closure, EntityKey, EntityUid};
use interlaken_ir::text::parse_catalog;

/// Table IDs of the fixture.
pub const USERS: TableId = TableId::new(200);
/// Groups.
pub const GROUPS: TableId = TableId::new(201);
/// Folders.
pub const FOLDERS: TableId = TableId::new(202);
/// Documents.
pub const DOCS: TableId = TableId::new(203);

/// Load the security fixture catalog.
pub fn catalog() -> InMemoryCatalog {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/security/catalog.ir");
    parse_catalog(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Construct a text datum.
pub fn t(s: &str) -> Datum {
    Datum::Text(s.into())
}

/// Return an integer key datum or NULL.
pub fn opt(v: Option<i64>) -> Datum {
    v.map_or(Datum::Null, Datum::Int8)
}

/// A document: id, owner, folder, title, secret, tags.
pub type DocRow = (
    i64,
    i64,
    Option<i64>,
    String,
    Option<i64>,
    Option<Vec<String>>,
);

/// Entity data for building a database.
#[derive(Clone, Debug, Default)]
pub struct Data {
    /// Users: id, name, dept, level.
    pub users: Vec<(i64, String, Option<String>, Option<i64>)>,
    /// Group IDs.
    pub groups: Vec<i64>,
    /// Folders: id and parent.
    pub folders: Vec<(i64, Option<i64>)>,
    /// Documents.
    pub docs: Vec<DocRow>,
    /// Memberships of users in groups.
    pub user_groups: Vec<(i64, i64)>,
}

/// Write the edges and closure of one hierarchy into its tables.
fn hierarchy(
    db: &mut Database,
    closure: &Closure,
    member: &str,
    group: &str,
    edges: TableId,
    closure_table: TableId,
) {
    let key = |k: &EntityKey| match k {
        EntityKey::Long(v) => Datum::Int8(*v),
        EntityKey::Str(s) => t(s),
    };
    db.set_rows(
        edges,
        closure
            .edges()
            .filter(|(m, g)| m.ty.as_str() == member && g.ty.as_str() == group)
            .map(|(m, g)| vec![key(&m.key), key(&g.key)])
            .collect(),
    );
    db.set_rows(
        closure_table,
        closure
            .pairs()
            .filter(|(d, a, _)| d.ty.as_str() == member && a.ty.as_str() == group)
            .map(|(d, a, n)| vec![key(&d.key), key(&a.key), Datum::Int8(n as i64)])
            .collect(),
    );
}

impl Data {
    /// Build the database, with closure tables maintained edge by edge.
    pub fn database(&self) -> Database {
        let mut db = Database::new();
        db.set_rows(
            USERS,
            self.users
                .iter()
                .map(|(id, name, dept, level)| {
                    vec![
                        Datum::Int8(*id),
                        t(name),
                        dept.as_deref().map_or(Datum::Null, t),
                        opt(*level),
                    ]
                })
                .collect(),
        );
        db.set_rows(
            GROUPS,
            self.groups
                .iter()
                .map(|g| vec![Datum::Int8(*g), t(&format!("g{g}"))])
                .collect(),
        );
        db.set_rows(
            FOLDERS,
            self.folders
                .iter()
                .map(|(f, _)| vec![Datum::Int8(*f), t(&format!("f{f}"))])
                .collect(),
        );
        db.set_rows(
            DOCS,
            self.docs
                .iter()
                .map(|(id, owner, folder, title, secret, tags)| {
                    vec![
                        Datum::Int8(*id),
                        Datum::Int8(*owner),
                        opt(*folder),
                        t(title),
                        opt(*secret),
                        tags.as_ref().map_or(Datum::Null, |ts| {
                            Datum::Array(ts.iter().map(|s| t(s)).collect())
                        }),
                    ]
                })
                .collect(),
        );
        let mut closure = Closure::new();
        for (u, g) in &self.user_groups {
            closure
                .add_edge(&EntityUid::long("User", *u), &EntityUid::long("Group", *g))
                .unwrap();
        }
        for (f, parent) in &self.folders {
            if let Some(p) = parent {
                closure
                    .add_edge(
                        &EntityUid::long("Folder", *f),
                        &EntityUid::long("Folder", *p),
                    )
                    .unwrap();
            }
        }
        for (d, _, folder, ..) in &self.docs {
            if let Some(f) = folder {
                closure
                    .add_edge(&EntityUid::long("Doc", *d), &EntityUid::long("Folder", *f))
                    .unwrap();
            }
        }
        hierarchy(
            &mut db,
            &closure,
            "User",
            "Group",
            TableId::new(204),
            TableId::new(205),
        );
        hierarchy(
            &mut db,
            &closure,
            "Folder",
            "Folder",
            TableId::new(206),
            TableId::new(207),
        );
        hierarchy(
            &mut db,
            &closure,
            "Doc",
            "Folder",
            TableId::new(208),
            TableId::new(209),
        );
        db
    }
}

/// Return the sample data.
///
/// users: 1 ann (eng, level 3) in staff; 2 bob (ops); 3 cy (level 1) in admins.
/// folders: 1 root; 2 and 3 inside 1.
/// docs: 10 by ann in folder 2 "a plan" secret 5 tags {a,b};
///       11 by bob in folder 3 "bad\" secret 42; 12 by bob, no folder, "notanumber" secret 7 tags {};
///       13 by cy in folder 1 "x", no secret, tags {a}.
pub fn sample() -> Data {
    let s = |x: &str| x.to_owned();
    Data {
        users: vec![
            (1, s("ann"), Some(s("eng")), Some(3)),
            (2, s("bob"), Some(s("ops")), None),
            (3, s("cy"), None, Some(1)),
        ],
        groups: vec![1, 2],
        folders: vec![(1, None), (2, Some(1)), (3, Some(1))],
        docs: vec![
            (
                10,
                1,
                Some(2),
                s("a plan"),
                Some(5),
                Some(vec![s("a"), s("b")]),
            ),
            (11, 2, Some(3), s("bad\\"), Some(42), None),
            (12, 2, None, s("notanumber"), Some(7), Some(vec![])),
            (13, 3, Some(1), s("x"), None, Some(vec![s("a")])),
        ],
        user_groups: vec![(1, 2), (3, 1)],
    }
}
