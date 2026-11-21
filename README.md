# Interlaken

Interlaken is an embedded, transactional relational database written in Rust, with authorization built into the query engine. Every statement runs as a *principal*, and the engine itself enforces row and column policies when it plans the statement, before the query runs, rather than leaving that to application code.

- **Storage:** MVCC transactions over [fjall](https://github.com/fjall-rs/fjall). It supports READ COMMITTED, REPEATABLE READ and SERIALIZABLE isolation, group commit, and selectable durability.
- **One typed relational IR:** query languages lower to a single IR, which the engine resolves, verifies, secures and optimizes. It has an S-expression text form, which is today's query language.
- **Security in the planner:** policies are written in [Cedar](https://www.cedarpolicy.com) (whose policies can be proved with an SMT solver) or in an RLS-style sublanguage, over entities and memberships stored in tables. They become filters, write checks and column masks inside the plan, and `EXPLAIN` hides policy logic from principals who aren't allowed to see it. Policy changes go through verified proposals with impact analysis.
- **A vectorized executor:** pull-based async streams of Arrow record batches, with per-query memory budgets and streaming results.

The project is pre-1.0. The on-disk format is not yet stable, and there is no SQL or network front end yet: clients use the Rust API.

## Example

```rust
use interlaken_engine::catalog::{ColumnSpec, TableSpec};
use interlaken_engine::{Engine, EngineOptions};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_ir::text::parse_type;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let admin = EntityUid::str("Admin", "root");
    let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;

    // A session runs one principal's statements; the creating administrator may run DDL.
    let mut session = engine.session(SecurityContext::new(admin));
    session
        .create_table(&TableSpec {
            name: "docs".into(),
            columns: vec![
                ColumnSpec::new("id", parse_type("int8")?),
                ColumnSpec::new("title", parse_type("text")?),
            ],
            primary_key: vec!["id".into()],
        })
        .await?;

    // Statements are IR text; each runs in its own transaction unless one is open.
    session
        .query(
            r#"(plan (insert docs (values (columns id title) (row 1 "draft") (row 2 "final")) (columns id title)))"#,
            Vec::new(),
        )
        .await?;
    let result = session
        .query(
            "(plan (project (filter (get docs :as d) (> d.id (param 1 (option int8)))) d.title))",
            vec![Datum::Int8(1)],
        )
        .await?;
    assert_eq!(result.rows(), vec![vec![Datum::Text("final".into())]]);
    Ok(())
}
```

## Workspace

| Crate | What it is |
| --- | --- |
| [`interlaken-storage`](crates/storage) | MVCC key-value storage on fjall: transactions, the commit sequencer, version GC, the tuple encoding, system catalog and row layer. |
| [`interlaken-ir`](crates/ir) | The typed relational IR: types, expressions and operators, the text format, resolver, verifier, optimizer, reference interpreter and the security core. |
| [`interlaken-exec`](crates/exec) | The vectorized executor: physical planning, Arrow operators, memory accounting, `EXPLAIN`. |
| [`interlaken-engine`](crates/engine) | The public API: `Engine` and `Session`, the storage-backed catalog and DDL, privileges, and versioned policies. |
| [`interlaken-test-alloc`](crates/test-alloc) | A counting allocator for memory-accounting tests (not published). |

## Building and testing

Interlaken needs Rust 1.90 or later.

```sh
cargo build --workspace
cargo test --workspace --release      # the full suite; release mode keeps the property and oracle tests fast
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --workspace --no-deps --open
cargo bench -p interlaken-engine      # executor benchmarks (ADR-0033)
UPDATE_BOOK=1 cargo test -p interlaken-engine --test book_reference   # regenerate the book's reference sections
```

The slow durability checks are scripts in [`scripts/`](scripts):

- `power-loss.sh`: power-loss testing on LazyFS (`build-lazyfs.sh` builds it).
- `disk-space-soak.sh`: a long sustained-update run that checks disk space levels off.
- `commit-stalls.sh`: fsync and commit latency on a target disk.

## Documentation

- **[The Interlaken book](docs/book)** (`mdbook serve docs/book`): how to use the database, the IR text language, the security model, transactions and operations. Its reference sections (functions, error codes, catalog tables, configuration) are generated from the code. Its IR examples are checked against an example database, and its Rust examples are compiled, all by `cargo test`.
- **API reference:** `cargo doc --workspace --no-deps --open`.
- **[Design records](docs/design):** the architectural decisions (ADRs) and design documents, with their history. They describe why things are the way they are; the book describes how they behave now.
- **[Backlog](docs/backlog.md):** known follow-up work.

## License

Apache-2.0.
