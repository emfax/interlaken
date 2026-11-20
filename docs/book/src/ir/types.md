# Types

> **Draft outline.** This chapter is not written yet.

## Will cover

- Scalar types: bool, int2/4/8, float8, text, bytea, date, timestamptz, uuid, jsonb.
- Nullability (`(not-null ...)`), arrays, records, entity references.
- Casts and coercions; which types tables can hold and which can be keys.

## Check against

- `crates/ir/src/types.rs`
- `crates/ir/src/text/read.rs`
- `crates/exec/src/`
- ADR-0003
- ADR-0024
