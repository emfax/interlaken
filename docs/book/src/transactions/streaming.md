# Streaming results

> **Draft outline.** This chapter is not written yet.

## Will cover

- `query_stream` and `QueryStream`: reading batches, `finish`, `collect`.
- How a stream ends: read to the end, an error, dropped early (queries stop; DML runs to its end in the background).

## Check against

- `crates/engine/src/session/stream.rs`
- `crates/engine/tests/session_streaming.rs`
- ADR-0031
