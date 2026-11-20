# Building Interlaken

Interlaken needs Rust 1.90 or later (edition 2024).

```sh
cargo build --workspace --release
```

## The workspace

| Crate | Directory | What it is |
| --- | --- | --- |
| `interlaken-storage` | `crates/storage` | MVCC transactions on fjall, the commit sequencer, version GC, the key encoding, the system catalog and the row layer |
| `interlaken-ir` | `crates/ir` | the typed relational IR: types, expressions, operators, the text format, the resolver, verifier, optimizer and reference interpreter, and the security core |
| `interlaken-exec` | `crates/exec` | the vectorized executor: physical planning, Arrow operators, memory accounting and `EXPLAIN` |
| `interlaken-engine` | `crates/engine` | the public API: `Engine` and `Session`, the storage-backed catalog and DDL, privileges and policies |
| `interlaken-test-alloc` | `crates/test-alloc` | a counting allocator for memory-accounting tests; not published |

An application depends on `interlaken-engine`, and on `interlaken-ir` for the types its API takes and returns: `Datum`, `SecurityContext`, `EntityUid` and the IR types. `interlaken-storage` provides `StorageOptions`, `Durability` and `TxnOptions`. See [Architecture](../concepts/architecture.md) for how the crates fit together.

The engine runs on [Tokio](https://tokio.rs). Its methods are `async`, and this book's examples run them under `#[tokio::main]`.

## Depending on Interlaken

The crates aren't published to a registry yet. Depend on them by path or git:

```toml
[dependencies]
interlaken-engine = { path = "../interlaken/crates/engine" }
interlaken-ir = { path = "../interlaken/crates/ir" }
interlaken-storage = { path = "../interlaken/crates/storage" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

### Features

`interlaken-engine` has one optional feature, `smt-prover`. It builds the SMT prover, which proves Cedar policy changes with Cedar's symbolic compiler and the [cvc5](https://cvc5.github.io) solver (see [Changing policies](../security/proposals.md)). Only *prover nodes* need it. An embedded database verifies policy changes with the gates every build has, and doesn't need it.

## Testing

```sh
cargo test --workspace --release
cargo clippy --workspace --all-targets -- -D warnings
```

Run the tests in release mode: the property, model and differential tests run many cases and are slow in debug builds. Besides unit and integration tests, the suite includes:

- **model-based tests** of storage against in-memory references;
- **history checking** of each isolation level with Jepsen's [Elle](https://github.com/jepsen-io/elle), which needs Java and the `elle-cli` jar (through `ELLE_CLI` or at `target/elle-cli.jar`); without them the check is skipped with a note;
- **crash tests** that kill a committing process and check what survives;
- **differential tests** that compare the optimizer and executor with the IR's reference interpreter;
- **a security oracle** that compares policy-governed results with unrestricted results filtered row by row;
- **this book:** its IR examples are checked against an example database, its Rust examples are compiled and run, and its generated reference sections are compared with the code.

Some tests are too slow for every run and are marked `#[ignore]`. Run them with `cargo test --release -- --ignored`:

| Test | What it does |
| --- | --- |
| `crates/exec/tests/ten_million.rs` | streams 10 million rows through the executor |
| `crates/engine/tests/streaming_memory.rs` | streams millions of rows through a session, measuring memory |
| `crates/engine/tests/closure_maintenance.rs` | measures closure maintenance on deep hierarchies (add `--nocapture`) |
| `crates/storage/tests/disk_space.rs` | the disk-space soak, run by `scripts/disk-space-soak.sh` |

The SMT prover's tests need the feature and a cvc5 1.3.1 executable, found through the `CVC5` environment variable or at `target/cvc5`. Without one, they note it and pass:

```sh
CVC5=/path/to/cvc5 cargo test --release -p interlaken-engine --features smt-prover --test smt_prover
```

The durability checks (power loss on a fault-injecting filesystem, disk-space soaks and commit stalls) are scripts in `scripts/`. See [Testing durability](../operations/durability-testing.md).

### Regenerating the book's reference sections

When a change adds a function, error code, catalog table or option, the book's reference test fails until the generated sections are rewritten:

```sh
UPDATE_BOOK=1 cargo test -p interlaken-engine --test book_reference
```

## Benchmarks

```sh
cargo bench -p interlaken-engine --bench executor          # throughput and peak memory per operator family
cargo bench -p interlaken-storage --bench commit_throughput  # commits per second by durability and concurrency
```

## Continuous integration

Two GitHub Actions workflows live in `.github/workflows`:

- **`nightly.yml`** runs at 03:00 UTC, on nights when `main` has new commits: clippy, the full test suite, the SMT prover tests, the slow ignored tests, the power-loss tests on LazyFS and the disk-space soak. Run it by hand from the Actions tab to skip the check for new commits.
- **`book.yml`** builds this book and publishes it to GitHub Pages whenever a change to `docs/book` reaches `main`.

## Documentation

```sh
cargo doc --workspace --no-deps --open   # the API reference
mdbook serve docs/book                   # this book, at http://localhost:3000
```
