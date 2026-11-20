# The statement pipeline

> **Draft outline.** This chapter is not written yet.

## Will cover

- Parse, resolve, verify, load the principal, load policies, secure, optimize, plan physically, execute.
- Which steps are cached (catalog by schema version, providers, residuals) and when the caches are bypassed.
- Pinned plans and schema versions.

## Check against

- `crates/engine/src/session/pipeline.rs`
- `crates/engine/src/catalog/cache.rs`
- ADR-0019
- ADR-0031
