# Syntax and names

> **Draft outline.** This chapter is not written yet.

## Will cover

- S-expressions: lists, symbols, strings, numbers, keywords (`:as`).
- The `(plan ...)` form; bindings and qualified column references (`d.id`).
- Unresolved and resolved forms; what clients may write (the verifier's rules, the reserved `sec$` prefix).
- Pretty-printing and round-tripping.

## Check against

- `crates/ir/src/text/`
- ADR-0013
- `crates/ir/src/verify/`

## The example database

The examples in this book run against a small database. A test checks that every IR example parses and resolves against it:

<!-- begin generated: example-schema -->
- `users`: `id` `(not-null int8)`, primary key; `name` `(not-null text)`; `dept` `text`.
- `folders`: `id` `(not-null int8)`, primary key; `name` `(not-null text)`; `owner` `int8`.
- `docs`: `id` `(not-null int8)`, primary key; `folder` `int8`; `title` `text`; `owner` `int8`; `updated` `timestamptz`; `tags` `(array text)`.
<!-- end generated: example-schema -->

Code blocks marked `ir` hold plans that resolve against this database. Blocks marked `ir,parse` only parse: fragments such as a single expression. Blocks marked `ir,error=CODE` fail with that SQLSTATE.

```ir
(plan (filter (get docs :as d) (> d.id (param 1 int8))))
```

```ir,parse
(call upper d.title)
```

```ir,error=42P01
(plan (get no_such_table))
```
