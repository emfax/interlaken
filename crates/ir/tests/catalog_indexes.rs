//! Secondary indexes in the catalog: text form, validation, and unique enforcement in the interpreter.

use interlaken_ir::catalog::{Catalog, IndexColumn};
use interlaken_ir::interp::{Database, Datum, EvalError, ExecOptions, execute};
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan};
use interlaken_ir::{ColumnId, TableId};

/// A table with every index form.
const CATALOG: &str = "(catalog (builtins)
  (table t :id 100
    (column id (not-null int8)) (column a int4) (column b text)
    (primary-key id)
    (index t_a a)
    (index t_ba (desc b) (asc a :nulls first))
    (index t_b b :unique true)))";

#[test]
/// Assert every index form parses into the expected definition.
fn index_forms_parse() {
    let catalog = parse_catalog(CATALOG).unwrap();
    let t = catalog.table_by_id(TableId::new(100)).unwrap();
    let [a, ba, b] = t.indexes.as_slice() else {
        panic!("expected three indexes")
    };
    assert_eq!((a.name.as_str(), a.unique), ("t_a", false));
    assert_eq!(a.columns, [IndexColumn::asc(ColumnId::new(1))]);
    assert_eq!(
        ba.columns,
        [
            IndexColumn::desc(ColumnId::new(2)),
            IndexColumn {
                column: ColumnId::new(1),
                descending: false,
                nulls_first: true
            }
        ]
    );
    assert!(b.unique);
}

#[test]
/// Assert malformed indexes are rejected.
fn malformed_indexes_are_rejected() {
    for (text, expected) in [
        ("(index t_x x)", "does not exist"),
        ("(index t_a a) (index t_a b)", "duplicate index"),
        ("(index t_x)", "no columns"),
        ("(index t_x (up a))", "expected asc or desc"),
    ] {
        let catalog = format!(
            "(catalog (table t :id 100 (column id (not-null int8)) (column a int4) (column b text) (primary-key id) {text}))"
        );
        let err = parse_catalog(&catalog)
            .err()
            .unwrap_or_else(|| panic!("{text} was accepted"));
        assert!(err.to_string().contains(expected), "{text}: {err}");
    }
}

#[test]
/// Assert the interpreter rejects duplicate values in a unique index, but not NULLs.
fn interpreter_enforces_unique_indexes() {
    let catalog = parse_catalog(CATALOG).unwrap();
    let insert = |rows: &str| {
        let text = format!("(plan (insert t (values (columns x y z) {rows}) (columns id a b)))");
        resolve(&parse_plan(&text).unwrap(), &catalog).unwrap().plan
    };
    let mut db = Database::new();
    let nulls = insert("(row 1 1 (cast null text)) (row 2 2 (cast null text)) (row 3 3 \"x\")");
    execute(&nulls, &catalog, &mut db, &[], ExecOptions::default()).unwrap();
    let duplicate = insert("(row 4 4 \"x\")");
    let err = execute(&duplicate, &catalog, &mut db, &[], ExecOptions::default()).unwrap_err();
    assert!(matches!(err, EvalError::UniqueViolation(_)), "{err}");
    assert_eq!(err.sqlstate(), "23505");
    assert_eq!(
        db.rows(TableId::new(100)).len(),
        3,
        "the failed statement changed nothing"
    );
    assert!(
        db.rows(TableId::new(100))
            .iter()
            .all(|r| r[2] != Datum::Text("y".into()))
    );
}
