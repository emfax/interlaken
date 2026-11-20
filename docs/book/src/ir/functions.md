# Built-in functions

> **Draft outline.** This chapter is not written yet.

## Will cover

- Every built-in scalar and aggregate function with its signature, null behaviour, volatility and leakproofness.
- To be generated from `builtin_functions()`, so it cannot drift from the code.
- Masking functions: `redact`, `mask_last4`.

## Check against

- `crates/ir/src/catalog/builtins.rs`
- ADR-0011
