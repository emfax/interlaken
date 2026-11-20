# Statements, atomicity and savepoints

> **Draft outline.** This chapter is not written yet.

## Will cover

- Each statement is atomic; a failed statement aborts an explicit transaction (`25P02`).
- Savepoints: `savepoint`, `rollback_to`, `release`.

## Check against

- `crates/exec/src/transaction.rs`
- `crates/engine/src/session/connection.rs`
- ADR-0026
