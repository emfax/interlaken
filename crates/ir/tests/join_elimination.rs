//! Join elimination through foreign keys (ADR-0035 phase 4): an inner join
//! from a child to its parent on a foreign key goes when nothing reads the
//! parent, and stays whenever it could change the result.

use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Database, Datum, ExecOptions, execute};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan, print_plan};
use interlaken_ir::verify::{VerifyOptions, verify};
use interlaken_ir::{Plan, TableId};

/// Users, documents with a plain owner and an optional editor, and
/// comments on documents, with a foreign key for each reference.
fn catalog() -> InMemoryCatalog {
    parse_catalog(
        "(catalog (builtins)
           (table users :id 100 (column id int8) (column name text) (primary-key id))
           (table docs :id 101
             (column id int8) (column owner int8) (column editor (option int8)) (column title text)
             (primary-key id) (index docs_owner owner) (index docs_editor editor))
           (table comments :id 102 (column id int8) (column doc int8) (primary-key id)
             (index comments_doc doc))
           (foreign-key docs_owner_fk :table docs (columns owner) (references users id))
           (foreign-key docs_editor_fk :table docs (columns editor) (references users id))
           (foreign-key comments_doc_fk :table comments (columns doc) (references docs id)))",
    )
    .unwrap()
}

/// Data that satisfies every foreign key.
fn data() -> Database {
    let mut db = Database::new();
    let text = |s: &str| Datum::Text(s.into());
    db.set_rows(
        TableId::new(100),
        vec![
            vec![Datum::Int8(1), text("ann")],
            vec![Datum::Int8(2), text("bob")],
        ],
    );
    db.set_rows(
        TableId::new(101),
        vec![
            vec![Datum::Int8(10), Datum::Int8(1), Datum::Int8(2), text("a")],
            vec![Datum::Int8(11), Datum::Int8(2), Datum::Null, text("b")],
            vec![Datum::Int8(12), Datum::Int8(2), Datum::Int8(1), text("c")],
        ],
    );
    db.set_rows(
        TableId::new(102),
        vec![
            vec![Datum::Int8(100), Datum::Int8(10)],
            vec![Datum::Int8(101), Datum::Int8(12)],
        ],
    );
    db
}

/// Resolve and optimize `text`; check the optimized plan verifies and
/// returns the same rows as the resolved one; return it.
fn optimized(text: &str) -> Plan {
    let catalog = catalog();
    let resolved = resolve(&parse_plan(text).unwrap(), &catalog)
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .plan;
    let optimized = optimize(&resolved, &catalog);
    verify(&optimized, &catalog, &VerifyOptions::default()).unwrap();
    let run = |plan: &Plan| {
        let mut rows = execute(plan, &catalog, &mut data(), &[], ExecOptions::default()).unwrap();
        rows.sort_by_key(|r| format!("{r:?}"));
        rows
    };
    assert_eq!(
        run(&resolved),
        run(&optimized),
        "{}",
        print_plan(&optimized)
    );
    optimized
}

/// Return how many joins `text` keeps after optimizing.
fn joins(text: &str) -> usize {
    print_plan(&optimized(text)).matches("(join").count()
}

#[test]
/// Assert a join to the parent on a plain foreign key goes when nothing
/// reads the parent, with the parent on either side, and transitively.
fn redundant_joins_go() {
    assert_eq!(
        joins(
            "(plan (project (join inner (get docs :as d) (get users :as u) (= d.owner u.id)) d.title))"
        ),
        0
    );
    assert_eq!(
        joins(
            "(plan (project (join inner (get users :as u) (get docs :as d) (= u.id d.owner)) d.id))"
        ),
        0
    );
    assert_eq!(
        joins(
            "(plan (project (join inner (join inner (get comments :as c) (get docs :as d) (= c.doc d.id))
               (get users :as u) (= d.owner u.id)) c.id))"
        ),
        0
    );
    // A filter on the child stays with the child.
    assert_eq!(
        joins(
            "(plan (project (filter (join inner (get docs :as d) (get users :as u) (= d.owner u.id))
               (= d.title \"a\")) d.id))"
        ),
        0
    );
}

#[test]
/// Assert the join stays whenever it could change what's read.
fn needed_joins_stay() {
    // The parent's columns are read.
    assert_eq!(
        joins(
            "(plan (project (join inner (get docs :as d) (get users :as u) (= d.owner u.id)) u.name))"
        ),
        1
    );
    // An optional referencing column: rows without an editor would drop.
    assert_eq!(
        joins(
            "(plan (project (join inner (get docs :as d) (get users :as u) (= d.editor u.id)) d.id))"
        ),
        1
    );
    // A condition beyond the foreign key restricts the parent.
    assert_eq!(
        joins(
            "(plan (project (join inner (get docs :as d) (get users :as u)
               (and (= d.owner u.id) (= u.name \"ann\"))) d.id))"
        ),
        1
    );
    // Not a foreign key: equal values, but nothing says the row exists.
    assert_eq!(
        joins(
            "(plan (project (join inner (get docs :as d) (get users :as u) (= d.id u.id)) d.id))"
        ),
        1
    );
    // The join's output is the plan's, consumed whole.
    assert_eq!(
        joins("(plan (join inner (get docs :as d) (get users :as u) (= d.owner u.id)))"),
        1
    );
    // A left join pads the child's referencing column.
    assert_eq!(
        joins(
            "(plan (project (join inner (join left (get comments :as c) (get docs :as d) (= c.doc d.id))
               (get users :as u) (= d.owner u.id)) c.id))"
        ),
        2
    );
}

#[test]
/// Assert a parent behind a policy barrier is never eliminated: the join
/// also drops children whose parent the principal can't see.
fn governed_parents_stay() {
    let catalog = catalog();
    let plan = parse_plan(
        "(plan :schema-version 1
           (project :bind 3
             (join inner
               (get (tid 101) :as d :bind 1)
               (policy-filter (get (tid 100) :as u :bind 2) (= #2.1 \"ann\"))
               (= #1.1 #2.0))
             (as #1.0 id)))",
    )
    .unwrap();
    verify(&plan, &catalog, &VerifyOptions { trusted: true }).unwrap();
    let optimized = print_plan(&optimize(&plan, &catalog));
    assert!(optimized.contains("(join inner"), "{optimized}");
}
