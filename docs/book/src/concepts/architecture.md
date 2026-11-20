# Architecture

> **Draft outline.** This chapter is not written yet.

## Will cover

- The four layers: storage, IR, executor, engine, and what each owns.
- The async/sync boundary: fjall calls on a storage pool.
- Where security happens, and why in the planner rather than the executor alone.
- Pointers into the design records for each layer.

## Check against

- `crates/*/src/lib.rs`
- ADR-0001
- ADR-0010
- ADR-0015
- ADR-0022
- ADR-0031
