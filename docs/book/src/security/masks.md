# Column policies and masks

> **Draft outline.** This chapter is not written yet.

## Will cover

- Column policies; `:mask null` and masking functions.
- Masks in queries, writes and `RETURNING`; what is refused with `42501`.
- Assignments from masked columns copy the masked value.

## Check against

- `crates/ir/src/security/plan.rs`
- `crates/engine/tests/session_masks.rs`
- ADR-0018
- ADR-0032
