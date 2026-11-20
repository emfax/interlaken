# Tables and column types

> **Draft outline.** This chapter is not written yet.

## Will cover

- `create_table`, `drop_table`, `add_column`; `TableSpec` and `ColumnSpec`.
- Reserved names (`sys_`, `engine_`); DDL needs the `admin` privilege.
- DDL is transactional, and conflicts with concurrent writers.

## Check against

- `crates/engine/src/catalog/`
- ADR-0007
- ADR-0031
