# Syntax and names

Interlaken has no SQL front end yet. Clients write statements in the *IR text format*: the S-expression form of the engine's typed relational IR. Every query language Interlaken will support lowers to this IR, so it is the most precise way to say what a statement does. It also round-trips: `Session::prepare` and `EXPLAIN` print plans in the same format, and that text can be run again.

A statement is a *plan*: one relational operator tree wrapped in `(plan ...)`.

```ir
(plan (project (filter (get docs :as d) (= d.owner 2)) d.id d.title))
```

Read it inside out: take the rows of `docs`, keep those whose `owner` is 2, and return their `id` and `title`. In SQL that is `SELECT d.id, d.title FROM docs AS d WHERE d.owner = 2`.

## Lists, atoms and keywords

- **Lists** are written `( ... )`. The first item names the operator or expression, such as `filter` or `=`, and the rest are its arguments.
- **Keyword arguments** are `:name value` pairs, such as `:as d` or `:count 10`. They may appear anywhere in a list, in any order, and each at most once. Everything else is positional.
- **Comments** start with `;` and run to the end of the line.
- **Integers** (`42`, `-7`) are `int8` literals. As explained under [Types](types.md#implicit-conversions), a bare integer adapts to the integer type it is compared with.
- **Floats** have a decimal point or an exponent (`1.5`, `2e10`) and are `float8` literals.
- **Strings** are `"text"`, with the escapes `\n`, `\t`, `\r`, `\\`, `\"` and `\u{hex}`.
- **Symbols** name tables, columns and functions. Literals of other types have their own forms, such as `(date "2026-01-31")`; see [Literals and expressions](expressions.md).

Whitespace separates atoms and is otherwise insignificant. Nesting is limited to 200 levels.

## Names

A bare symbol is a name: `users`, `title`. A dotted symbol is a qualified name: `d.title` is the column `title` of whatever `d` names. Names are case-sensitive, and are matched exactly.

A segment that isn't a plain word can be quoted with bars: `|odd name|` or `d.|Title|`. Inside bars, `\|` is a bar and the string escapes apply. `true`, `false` and `null` are literals, so a column with one of those names must be quoted, as in `t.|null|`.

```ir,parse
(project (get users :as u) u.|name|)
```

### Tables and aliases

`(get TABLE)` reads a table. `:as` gives it an alias, which qualifies its columns. Without an alias, a table's columns are qualified by the table name, so these mean the same:

```ir
(plan (project (get users) users.name))
(plan (project (get users :as u) u.name))
```

### Column references

- **Qualified:** `q.c` matches column `c` of the `get` aliased or named `q`, or of a DML target.
- **Unqualified:** `c` is looked up in the operator's input first. If it isn't found there, the lookup continues through enclosing scopes: the outer side of an `apply`, or the query around a subquery.
- **Ambiguous:** if a name matches more than one visible column of the same scope, the plan fails with `42702`.

```ir,error=42702
(plan (project (join inner (get users :as u) (get folders :as f) (= f.owner u.id)) id))
```

### Output column names

Every operator's output columns have names, which a later operator or the client sees:

- **`(as EXPR NAME)`** names an output item explicitly.
- **A bare column reference** keeps the column's name.
- **Any other unnamed expression** has no name. Clients see it as `?column?`, as in Postgres.
- **Aggregates** are named after their function unless renamed, so `(agg count)` outputs a column named `count`.

Output names don't have to be unique: projecting `d.id` and `f.id` gives two columns named `id`. Use `as` to tell them apart.

```ir
(plan (project (get users :as u) u.name (+ u.id 1) (as (* u.id 2) twice)))
```

The output columns are `name`, `?column?` and `twice`.

## Unresolved and resolved forms

What clients write is *unresolved* IR: it refers to tables, columns and functions by name. The engine resolves each plan against the catalog. Resolution does the following:

- looks up every name;
- types every expression;
- inserts the implicit conversions as explicit casts;
- lowers subqueries.

The result is *resolved* IR, in which references are by ID:

| Unresolved | Resolved | Meaning |
| --- | --- | --- |
| `users` | `(tid 111)` | a table, by catalog ID |
| `upper` | `(fid 2)` | a function overload, by catalog ID |
| `u.name` | `#1.1` | column 1 of binding 1 |
| (column name in DML) | `(cid 3)` | a target column, by ID |
| | `:bind N` | the binding an operator's output columns get |
| | `:target N` | the binding of a DML target's row |
| | `:schema-version N` | the catalog version the plan was resolved against |

`Session::prepare` returns the resolved form, pinned to the catalog's schema version. Printed, it looks like this:

```text
(plan :schema-version 2
  (project :bind 2
    (filter (get (tid 111) :as u :bind 1) (= #1.0 (param 1 (option int8))))
    (as #1.1 name)
    (as (call (fid 2) #1.1) shout)))
```

Resolved text can be run again unchanged, until DDL changes the schema version (see [Parameters and prepared plans](parameters.md)). Clients rarely write the resolved form by hand. The IDs depend on the database, so the examples in this book use names.

## What clients can't write

Every plan passes the *verifier* before it runs. For plans from clients, the verifier rejects with `42501`:

- `policy-filter` and `write-check`, the operators security lowering adds;
- `try`, the error-catching expression that lowering uses for policies;
- aliases and `values` column names starting with `sec$`, the prefix that marks what lowering adds;
- writes to the catalog tables.

The engine's own passes may produce all of these. A client can't, so it can't forge a policy or hide part of its plan from `EXPLAIN`.

```ir,error=42501
(plan (get users :as sec$u))
```

## Errors

Text that doesn't parse fails with `42601`. The message gives the line and column, such as `1:32: unknown expression 'policy-filter'`. A plan that parses but doesn't make sense fails during resolution:

| Code | When |
| --- | --- |
| `42P01` | an unknown table |
| `42703` | an unknown column |
| `42702` | an ambiguous column |
| `42883` | no function of that name accepts the argument types |
| `42804` | an expression of the wrong type |
| `0A000` | something not supported yet |
| `42000` | any other invalid plan, such as an `insert` that names too few columns |

The full list is in [Error codes](../reference/errors.md).

```ir,error=42P01
(plan (get no_such_table))
```

## The example database

The examples in this book run against a small database. A test checks that every IR example parses and resolves against it:

<!-- begin generated: example-schema -->
- `users`: `id` `int8`, primary key; `name` `text`; `dept` `(option text)`.
- `folders`: `id` `int8`, primary key; `name` `text`; `owner` `(option int8)`.
- `docs`: `id` `int8`, primary key; `folder` `(option int8)`; `title` `(option text)`; `owner` `(option int8)`; `updated` `(option timestamptz)`; `tags` `(option (array (option text)))`.
- `groups`: `id` `int8`, primary key; `name` `text`.
- `user_groups`: `member` `int8`, primary key; `grp` `int8`, primary key.
- `user_group_closure`: `descendant` `int8`, primary key; `ancestor` `int8`, primary key; `paths` `int8`.

Entity types, for policies:

- `User`: rows of `users`, keyed by `id`, with attributes `name`, `dept`.
- `Group`: rows of `groups`, keyed by `id`, with attributes `name`.
- `Folder`: rows of `folders`, keyed by `id`, with attributes `name`, `owner` (a `User`).
- `Doc`: rows of `docs`, keyed by `id`, with attributes `title`, `owner` (a `User`), `folder` (a `Folder`), `tags`.

Membership: a `User` can be in a `Group`. Direct memberships are rows of `user_groups` (`member`, `grp`), and `user_group_closure` holds every transitive pair with its path count.
<!-- end generated: example-schema -->

Code blocks in this book are marked by what they need to do:
- **`ir`:** plans that resolve against this database.
- **`ir,parse`:** fragments, such as a single operator, that need only parse.
- **`ir,error=CODE`:** plans that fail with that SQLSTATE.
- **`text`:** output, which isn't checked.
