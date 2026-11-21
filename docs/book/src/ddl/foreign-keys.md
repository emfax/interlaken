# Foreign keys

A foreign key says that some columns of one table hold a key of another table, or of the same table. A document's `owner` holds a user's `id`; a folder's `parent` holds another folder's `id`. Declaring it lets Interlaken check that every reference leads somewhere.

> **Cascades aren't built yet.** `on_delete` actions other than `NoAction` aren't carried out: a delete that `Cascade` or `SetNull` should handle fails as `NoAction` would, and its message says so. That is phase 3 of [ADR-0035](../reference/design-records.md).

## Creating a foreign key

```rust
# use interlaken_engine::catalog::{
#     ColumnSpec, ForeignKeySpec, IndexKey, IndexSpec, ReferentialAction, TableSpec,
# };
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# session.create_table(&TableSpec {
#     name: "users".into(),
#     columns: vec![ColumnSpec::new("id", parse_type("int8")?)],
#     primary_key: vec!["id".into()],
# }).await?;
# session.create_table(&TableSpec {
#     name: "folders".into(),
#     columns: vec![
#         ColumnSpec::new("id", parse_type("int8")?),
#         ColumnSpec::new("owner", parse_type("int8")?),
#         ColumnSpec::new("parent", parse_type("(option int8)")?),
#     ],
#     primary_key: vec!["id".into()],
# }).await?;
// Deletes of a user or folder find the rows that refer to it through these.
for (name, column) in [("folders_owner", "owner"), ("folders_parent", "parent")] {
    session
        .create_index(&IndexSpec {
            name: name.into(),
            table: "folders".into(),
            columns: vec![IndexKey::asc(column)],
            unique: false,
        })
        .await?;
}
session
    .create_foreign_key(&ForeignKeySpec::new(
        "folders_owner_fk", "folders", ["owner"], "users", ["id"],
    ))
    .await?;
session
    .create_foreign_key(
        &ForeignKeySpec::new("folders_parent_fk", "folders", ["parent"], "folders", ["id"])
            .on_delete(ReferentialAction::Cascade),
    )
    .await?;
# session.drop_foreign_key("folders_parent_fk").await?;
# Ok(())
# }
```

A `ForeignKeySpec` has:

- **`name`:** unique among foreign keys. A duplicate fails with `42P16`.
- **`table`, `columns`:** the referencing table and columns.
- **`references`, `referenced_columns`:** the referenced table, which may be the same table, and its columns, paired with `columns` by position.
- **`on_delete`:** what deleting a referenced row does to the rows that refer to it. `NoAction`, the default, fails the statement if referencing rows remain when it ends. `Cascade` deletes them. `SetNull` sets their referencing columns to NULL.
- **`match_full`:** how NULLs are treated. By default (`MATCH SIMPLE`), a row with any NULL referencing column isn't checked. With `match_full()` (`MATCH FULL`), the referencing columns must be all NULL or all present.

The declaration must fit its tables, or `create_foreign_key` fails with `42P16`:

- The referenced columns are the referenced table's primary key, or exactly the columns of one of its unique indexes, in any order.
- Each referencing column has the same type as the column it refers to, apart from optionality: an `(option int8)` column can refer to an `int8` key.
- The referencing columns lead the referencing table's primary key or one of its indexes, in any order. Deleting a referenced row finds the rows that refer to it through that index, so the error names the columns an index needs. A table keyed by the entity it describes, such as `users_phone(user_id, phone)` with primary key `user_id`, needs no other index.
- `SetNull` needs referencing columns of optional type.
- Neither table is a catalog table.
- If the referencing table backs an entity type whose attribute in the referencing column `references` an entity type, the foreign key refers to that entity type's table and key (see [Entities and memberships](entities.md)). The same check applies when the entity type is declared after the foreign key.

A column that doesn't exist fails with `42703`.

## Existing rows

Creating a foreign key checks every row of the referencing table in the same statement. A row that refers to a row that doesn't exist fails the statement with `23503`, `foreign_key_violation`, and the message names the missing key:

```text
foreign key violation: foreign key folders_owner_fk: a row of folders refers to a row of users with id = Int8(7), which doesn't exist
```

Under `MATCH FULL`, a row with some but not all referencing columns NULL fails the same way. Nothing is recorded when the check fails.

## Checking writes

Every insert, update and delete is checked against the foreign keys from and to the table it writes:

- **A new reference** must name an existing row. That covers an inserted row, and an updated row whose referencing columns changed, unless the match rule exempts it because of NULLs.
- **A removed row** must leave nothing referring to it. That covers a deleted row, and an updated row whose referenced columns changed. Updates that change neither side aren't checked.

Each must hold when the statement ends, not after each row. So one statement may insert a tree into a self-referencing table in any order, children before parents, or delete a whole subtree. A row may refer to itself. A violation fails the statement with `23503`, and the statement's writes are undone ([Statements, atomicity and savepoints](../transactions/statements.md)):

```text
foreign key violation: foreign key folders_owner_fk: a row of folders refers to a row of users with id = Int8(7), which doesn't exist
foreign key violation: foreign key folders_parent_fk: rows of folders still refer to the row of folders with id = Int8(10)
```

Checks aren't deferred to commit. Two tables that refer to each other need one side to start NULL, filled in by a later statement.

The checks run as each batch of rows is written, using the index described above to find referring rows. A key that fails is rechecked when the statement ends, so in the usual case nothing is held in memory. Keys waiting for a recheck count against the query's memory limit (see [Memory](../operations/memory.md)). `EXPLAIN` lists the foreign keys a write checks: `checking foreign keys: folders_owner_fk, folders_parent_fk`.

### Concurrency

The checks hold at every isolation level, not just `serializable`. Suppose one transaction inserts a folder owned by user 5 while another deletes user 5: whichever commits second fails with `40001`. Each check registers the rows and index ranges it read as dependencies of its transaction, as unique constraints and hierarchy maintenance do. A retry then sees the other transaction's write, and either succeeds or fails with `23503`.

A row's reference also depends on the referenced row as a whole. So inserting a folder owned by user 5 also conflicts with a concurrent update of any of user 5's columns, and one of the two transactions retries.

### Policies

Checks read rows whatever the writer's policies allow, because a constraint can't mean different things to different principals. This tells a writer whether a row they can't read exists:
- inserting a row that refers to key 7 succeeds only if a row with key 7 exists;
- deleting a row fails while rows the writer can't see still refer to it.

Unique indexes reveal existence in the same way, as they do in Postgres. Error messages carry only the writer's own values and the names of tables and foreign keys, never values from rows the writer can't read. Don't put a foreign key across a boundary that must hide whether rows exist.

## Dropping

`drop_foreign_key(name)` removes a foreign key; its tables and indexes stay. A foreign key also limits other DDL:

- Dropping a table that another table's foreign key refers to fails with `42P16`. Drop the foreign key first. Dropping the referencing table drops its own foreign keys, a self-reference included.
- Dropping the unique index a foreign key refers to fails with `42P16`.
- Dropping the referencing table's last index that leads with the referencing columns fails with `42P16`. Create another such index first.

Foreign key DDL needs `admin` and changes the schema version, like all DDL. The definitions are kept in the catalog tables `engine_foreign_keys` and `engine_foreign_key_columns` (see [Catalog tables](../reference/catalog-tables.md)).
