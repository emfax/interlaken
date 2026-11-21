//! Foreign keys in the catalog (ADR-0035): text form, lookups and validation.

use interlaken_ir::catalog::{Catalog, ReferentialAction, referencing_index};
use interlaken_ir::text::parse_catalog;
use interlaken_ir::{ColumnId, TableId};

/// Tables a foreign key can join: `users` with a unique `email`, and
/// `docs` with indexes on `owner` and `(parent, id)`.
const TABLES: &str = "
  (table users :id 100
    (column id int8) (column email (option text)) (column team int4)
    (primary-key id)
    (index users_email email :unique true)
    (index users_team team))
  (table docs :id 101
    (column id int8) (column owner (option int8)) (column editor (option text))
    (column parent (option int8)) (column team int8)
    (primary-key id)
    (index docs_owner owner)
    (index docs_editor editor)
    (index docs_parent parent id)
    (index docs_team team))";

/// Parse a catalog of [`TABLES`] and `items`, returning the error message on failure.
fn catalog(items: &str) -> Result<interlaken_ir::catalog::InMemoryCatalog, String> {
    parse_catalog(&format!("(catalog {TABLES} {items})")).map_err(|e| e.to_string())
}

#[test]
/// Assert every form parses into the expected definition, and lookups find it.
fn forms_parse() {
    let catalog = catalog(
        "(foreign-key docs_owner_fk :table docs (columns owner) (references users id)
           :on-delete cascade)
         (foreign-key docs_editor_fk :table docs (columns editor) (references users email)
           :index users_email :on-delete set-null :match full)
         (foreign-key docs_parent_fk :table docs (columns parent) (references docs id))",
    )
    .unwrap();
    let fks = catalog.foreign_keys();
    let [editor, owner, parent] = fks.as_slice() else {
        panic!("expected three foreign keys")
    };
    assert_eq!(owner.name.as_str(), "docs_owner_fk");
    assert_eq!(
        (owner.table, owner.referenced),
        (TableId::new(101), TableId::new(100))
    );
    assert_eq!(owner.columns, [ColumnId::new(1)]);
    assert_eq!(owner.referenced_columns, [ColumnId::new(0)]);
    assert_eq!(owner.referenced_index, None);
    assert_eq!(owner.on_delete, ReferentialAction::Cascade);
    assert!(!owner.match_full);
    assert_eq!(
        editor.referenced_index.as_ref().map(|i| i.as_str()),
        Some("users_email")
    );
    assert_eq!(editor.on_delete, ReferentialAction::SetNull);
    assert!(editor.match_full);
    assert_eq!(parent.on_delete, ReferentialAction::NoAction);

    let docs = catalog.table_by_id(TableId::new(101)).unwrap();
    assert_eq!(
        referencing_index(owner, &docs).map(|i| i.map(|n| n.as_str())),
        Some(Some("docs_owner"))
    );
    assert_eq!(
        referencing_index(parent, &docs).map(|i| i.map(|n| n.as_str())),
        Some(Some("docs_parent"))
    );
    assert_eq!(catalog.foreign_keys_of(TableId::new(100)).len(), 0);
    assert_eq!(catalog.foreign_keys_of(TableId::new(101)).len(), 3);
    assert_eq!(catalog.foreign_keys_referencing(TableId::new(100)).len(), 2);
    assert_eq!(catalog.foreign_keys_referencing(TableId::new(101)).len(), 1);
}

#[test]
/// Assert declarations that don't fit their tables or each other are rejected, with the reason.
fn invalid_declarations_are_rejected() {
    let cases = [
        (
            "(foreign-key f :table users (columns team) (references users team))",
            "the referenced columns are not the primary key of users",
        ),
        (
            "(foreign-key f :table docs (columns team) (references users id))
             (foreign-key f :table docs (columns team) (references users id))",
            "invalid foreign key f: already declared",
        ),
        (
            "(foreign-key f :table docs (columns editor) (references users email) :index users_team)",
            "not the columns of the unique index users_team",
        ),
        (
            "(foreign-key f :table docs (columns owner) (references users email) :index users_email)",
            "docs.owner has type int8, but the column it refers to, users.email, has type text",
        ),
        (
            "(foreign-key f :table users (columns team) (references docs id))",
            "users.team has type int4",
        ),
        (
            "(foreign-key f :table docs (columns id) (references users id) :on-delete set-null)",
            "on delete set-null needs docs.id to have an optional type",
        ),
        (
            "(foreign-key f :table docs (columns owner owner) (references users id id))",
            "its referencing columns repeat a column",
        ),
        (
            "(foreign-key f :table docs (columns owner parent) (references users id))",
            "it has 2 referencing columns but 1 referenced columns",
        ),
        (
            "(foreign-key f :table docs (columns) (references users))",
            "it has no columns",
        ),
        (
            "(foreign-key f :table users (columns id) (references users id) :on-delete sometimes)",
            "expected no-action, cascade or set-null",
        ),
        (
            "(foreign-key f :table users (columns id) (references users id) :match partial)",
            "expected simple or full",
        ),
    ];
    for (items, expected) in cases {
        let err = catalog(items).unwrap_err();
        assert!(err.contains(expected), "{items}: {err}");
    }
}

#[test]
/// Assert the referencing columns must lead the primary key or an index, in any order.
fn referencing_columns_need_an_index() {
    let tables = "
      (table pairs :id 100 (column a int8) (column b int8) (primary-key a b))
      (table refs :id 101 (column id int8) (column a int8) (column b int8) (primary-key id))
      (table keyed :id 102 (column b int8) (column a int8) (column n int8) (primary-key b a n))";
    let parse = |items: &str| {
        parse_catalog(&format!("(catalog {tables} {items})")).map_err(|e| e.to_string())
    };
    let err =
        parse("(foreign-key f :table refs (columns a b) (references pairs a b))").unwrap_err();
    assert!(
        err.contains("refs needs an index whose leading columns are a, b"),
        "{err}"
    );
    // Leading primary key columns in another order serve.
    parse("(foreign-key f :table keyed (columns a b) (references pairs a b))").unwrap();
    // Referenced columns may be listed in any order of the key.
    parse("(foreign-key f :table keyed (columns b a) (references pairs b a))").unwrap();
}

#[test]
/// Assert a foreign key must agree with an entity attribute on the same column.
fn references_must_agree() {
    let items = "(entity User :table users :key id)
       (entity Doc :table docs :key id (attr owner :references User))";
    catalog(&format!(
        "{items} (foreign-key f :table docs (columns owner) (references users id))"
    ))
    .unwrap();
    let err = catalog(&format!(
        "{items} (foreign-key f :table docs (columns parent) (references docs id))
         (entity Folder :table docs :key id)"
    ))
    .unwrap_err();
    assert!(
        err.contains("its table already backs an entity type"),
        "{err}"
    );
    let err = catalog(
        "(entity User :table users :key id)
         (entity Doc :table docs :key id (attr parent :references User))
         (foreign-key f :table docs (columns parent) (references docs id))",
    )
    .unwrap_err();
    assert!(
        err.contains("attribute parent of entity type Doc refers to entity type User"),
        "{err}"
    );
    let err = catalog(
        "(foreign-key f :table docs (columns parent) (references docs id))
         (entity User :table users :key id)
         (entity Doc :table docs :key id (attr parent :references User))",
    )
    .unwrap_err();
    assert!(err.contains("invalid entity declaration Doc"), "{err}");
}
