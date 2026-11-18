//! The resolver fixture catalog and a small sample dataset for it.

#![allow(dead_code)]

use std::path::Path;

use interlaken_ir::TableId;
use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Database, Datum};
use interlaken_ir::text::parse_catalog;

/// Load the resolver fixture catalog.
pub fn catalog() -> InMemoryCatalog {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resolve/catalog.ir");
    parse_catalog(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Construct a text datum.
pub fn t(s: &str) -> Datum {
    Datum::Text(s.into())
}

/// Return the sample data.
///
/// users: (1 ann ann@x 30 10 tags {a,b}), (2 bob NULL 17 0 tags NULL), (3 cy cy@x NULL 5 tags {}).
/// posts: (10 by 1 "p1" 5 likes score 1.5), (11 by 1 "p2" 7 likes), (12 by 2 "p3" 1 like).
/// audit: (100 user 1 "login" level 1), (101 user NULL "boot" level NULL).
pub fn data() -> Database {
    use Datum::{Array, Float8, Int2, Int4, Int8, Null, Timestamptz};
    let mut db = Database::new();
    db.set_rows(
        TableId::new(100),
        vec![
            vec![
                Int8(1),
                t("ann"),
                t("ann@x"),
                Int4(30),
                Int8(10),
                Null,
                Null,
                Array(vec![t("a"), t("b")]),
            ],
            vec![Int8(2), t("bob"), Null, Int4(17), Int8(0), Null, Null, Null],
            vec![
                Int8(3),
                t("cy"),
                t("cy@x"),
                Null,
                Int8(5),
                Null,
                Null,
                Array(vec![]),
            ],
        ],
    );
    db.set_rows(
        TableId::new(101),
        vec![
            vec![
                Int8(10),
                Int8(1),
                t("p1"),
                Int4(5),
                Float8(1.5),
                Timestamptz(0),
            ],
            vec![Int8(11), Int8(1), t("p2"), Int4(7), Null, Timestamptz(0)],
            vec![Int8(12), Int8(2), t("p3"), Int4(1), Null, Timestamptz(0)],
        ],
    );
    db.set_rows(
        TableId::new(103),
        vec![
            vec![Int8(100), Int8(1), t("login"), Int2(1)],
            vec![Int8(101), Null, t("boot"), Null],
        ],
    );
    db
}
