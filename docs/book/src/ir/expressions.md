# Literals and expressions

> **Draft outline.** This chapter is not written yet.

## Will cover

- Literals for every type, including `(date ...)`, `(timestamptz ...)`, `(uuid ...)`, `(bytea ...)`.
- Operators, `and`/`or`, `case`, `coalesce`, `nullif`, `cast`, `try`, `array`, `call`.
- Postgres semantics: three-valued logic, NULL handling, overflow and division errors.

## Check against

- `crates/ir/src/text/read.rs`
- `crates/ir/src/text/literal.rs`
- `crates/ir/src/interp/`
- ADR-0024
