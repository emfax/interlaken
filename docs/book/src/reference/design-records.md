# Design records

The design records in `docs/design` explain why Interlaken is built the way it is. They are architectural decision records (ADRs), numbered in the order they were written, and four longer design documents.

ADRs differ from this book:

- **This book describes current behaviour,** and is kept true to the code: its examples are checked and its reference sections are generated.
- **ADRs record decisions and their history:** the context, the options considered, the decision and its consequences. As each part is built, an ADR gains *As implemented* notes where the implementation differs from the plan, and they are kept rather than rewritten. Where an ADR and this book disagree, this book is right about the current behaviour.

An ADR's status is *proposal* while it is under discussion, *accepted* when its decision is made but not fully built, *implemented* once it is built, and *superseded* when a later ADR replaces it.

## Storage

| ADR | Title | Status |
| --- | --- | --- |
| 0001 | fjall storage engine and the async/sync boundary | implemented |
| 0002 | Engine-owned MVCC and keyspace layout | implemented |
| 0003 | Order-preserving tuple encoding and row format | implemented |
| 0004 | Optimistic transactions and isolation levels | implemented |
| 0005 | Commit sequencer, group commit and durability | implemented |
| 0006 | Batched async storage trait | implemented |
| 0007 | Minimal catalog and row layer | implemented |
| 0008 | Version garbage collection | implemented |
| 0009 | Storage correctness testing strategy | implemented |
| 0037 | Replication with Raft | implemented |
| 0038 | Streaming snapshots | implemented |

## IR and optimizer

| ADR | Title | Status |
| --- | --- | --- |
| 0010 | Typed relational IR as the single middle end | implemented |
| 0011 | Function property catalog | implemented |
| 0012 | Nesting and decorrelation for GraphQL | implemented |
| 0013 | IR forms, formats, resolver and verifier | implemented |
| 0014 | Rule-based optimizer for the MVP | implemented |
| 0021 | IR correctness and security testing strategy | implemented |
| 0034 | Two-valued logic with an explicit option type | accepted: built except SQL lowering, which waits for the SQL front end |
| 0036 | Statistics and cost-based planning | proposal |

## Security

| ADR | Title | Status |
| --- | --- | --- |
| 0015 | Security core and policy provider contract | implemented |
| 0016 | Security expression sublanguage | implemented |
| 0017 | Entities and hierarchy closure table | implemented |
| 0018 | PolicyFilter barrier and WriteCheck plan constructs | implemented |
| 0019 | Transactional authorization and residual cache | implemented |
| 0020 | Cedar policy provider and deployment gates | implemented |
| 0030 | Policy change proposals and distributed verification | implemented |
| 0032 | Authorization completeness: closure maintenance, entity loading, column masks and impact at scale | implemented |

## Executor

| ADR | Title | Status |
| --- | --- | --- |
| 0022 | Vectorized Arrow stream executor | implemented |
| 0023 | Leaf operators and storage pushdown | accepted: pushdown and index access are built; decoding on the storage thread is not (backlog `batch-decoder`) |
| 0024 | Expression evaluation with Postgres semantics | implemented |
| 0025 | Runtime security enforcement in the executor | implemented |
| 0026 | Statement atomicity and Halloween protection | implemented |
| 0027 | Query memory management | implemented |
| 0028 | Physical operators, planning and EXPLAIN | implemented |
| 0029 | WASM UDF interface | accepted: designed, not built |
| 0033 | Executor performance | implemented |

## Engine

| ADR | Title | Status |
| --- | --- | --- |
| 0031 | Engine sessions and the storage-backed catalog | implemented |
| 0035 | Enforced foreign keys | implemented |
| 0039 | Telemetry and cost attribution | accepted: storage, statement and replication telemetry are built |

## Design documents

| Document | What it is |
| --- | --- |
| `mvp-fjall-storage-design.md` | the storage layer's design proposal, from which ADR-0001 to ADR-0009 came |
| `ir-design.md` | the IR and security core's design proposal (ADR-0010 to ADR-0021) |
| `executor-design.md` | the executor's design proposal (ADR-0022 to ADR-0029) |
| `mvp-completion.md` | the final milestones that made the layers a usable embedded database, and what comes after the MVP |

`docs/design/0000-template.md` is the template for new ADRs. Follow-up work that comes out of the ADRs is tracked in `docs/backlog.md`.
