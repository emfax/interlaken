# Introduction

Interlaken is an embedded, transactional relational database written in Rust, whose query engine enforces authorization. An application links it as a library, opens a database directory, and runs statements through sessions. Each session belongs to one *principal*, a user or service the application has authenticated. The engine compiles the active policies into every statement's plan before the statement runs, so a query returns only the rows and columns its principal may read, and a write changes only what its principal may change. Application code doesn't need to remember to filter.

Its main parts:

- **Storage:** multi-version transactions over [fjall](https://github.com/fjall-rs/fjall), an LSM-tree key-value store. They support READ COMMITTED, REPEATABLE READ and SERIALIZABLE isolation, with group commit and a choice of durability.
- **A typed relational IR:** every statement is a plan in one typed relational algebra. Today, clients write it in an S-expression text form, the *IR text language*. Front ends for SQL and GraphQL are planned to lower into the same IR.
- **Security in the planner:** policies are written in [Cedar](https://www.cedarpolicy.com) or in an RLS-style sublanguage, over *entities* and *memberships* stored in ordinary tables. The engine turns them into filters, write checks and column masks inside the plan. Policy changes go through verified proposals, with impact analysis against the current data.
- **A vectorized executor:** async streams of Arrow record batches, with per-query memory budgets and streaming results.

## Who it is for

Interlaken is for Rust applications that want a relational database inside the process, and want authorization to be the database's job rather than a convention every query must follow.

It is pre-1.0. In particular:

- **No SQL and no network access yet.** Clients use the Rust API and write statements in the IR text language, or build plans with the IR's Rust builder.
- **No stable on-disk format.** A database written by one version may not open with another; it fails with a format version error rather than being misread.
- **Single node.** Replication is planned but not started.

[Limits and unsupported features](reference/limits.md) lists the rest.

## How this book is organised

- **[Getting started](getting-started/building.md):** building Interlaken, a first database, and sessions.
- **[Concepts](concepts/architecture.md):** the crates and how a statement travels through them.
- **[The IR text language](ir/syntax.md):** the query language, its types, expressions and operators.
- **[Data definition](ddl/tables.md):** tables, indexes, entities and memberships.
- **[Security](security/model.md):** how policies govern statements, how to write them, and how to change them safely.
- **[Transactions](transactions/isolation.md):** isolation, atomicity, streaming and timeouts.
- **[Operations](operations/configuration.md):** configuration, durability, memory, disk space and metrics.
- **[Reference](reference/errors.md):** error codes, catalog tables, limits, a glossary and the design records.

Two other sources complement it:

- **The API reference,** built with `cargo doc --workspace --no-deps --open`, documents every public type and method.
- **The design records** in `docs/design` explain why Interlaken is built the way it is, and how it got there (see [Design records](reference/design-records.md)). Where a design record and this book disagree, this book describes the current behaviour.

## Conventions

- **IR examples** are in the IR text language. `cargo test` checks every one against this book's [example database](ir/syntax.md#the-example-database): an example must parse, resolve and verify, and an example shown failing must fail with the code given.
- **Rust examples** are compiled and run by `cargo test`, except fragments marked as such.
- **Errors** are identified by their SQLSTATE code, such as `42501` (insufficient privilege). Interlaken uses Postgres's codes and meanings (see [Error codes](reference/errors.md)).
- **Postgres comparisons:** Interlaken follows Postgres's semantics for NULLs, types, expressions and transactions unless this book says otherwise, and points out where it differs.
- **Generated sections:** the function, error, catalog table and configuration references are generated from the code, and `cargo test` fails if they drift from it.
