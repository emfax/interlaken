# Inserting, updating and deleting

> **Draft outline.** This chapter is not written yet.

## Will cover

- `insert`, `update`, `delete` with `returning`.
- Constraints: primary key, unique, NOT NULL; their SQLSTATEs.
- Statement atomicity and Halloween protection.
- Writes to membership edges tables maintain closures.

## Check against

- `crates/exec/src/operators/dml.rs`
- `crates/exec/src/closure.rs`
- ADR-0026
- ADR-0032
