# Catalog tables

The catalog lives in ordinary tables, written in the same transactions as the changes they record, so DDL is transactional:

- **Storage's `sys_*` tables** hold what storage needs to read and write rows: tables, columns and indexes.
- **The engine's `engine_*` tables** hold what the IR needs on top: IR column types, entity and membership declarations, function properties, privileges, the schema version and the policies.

Clients can't read or write any of them. Names starting with `sys_` or `engine_` are reserved, the IR catalog doesn't show these tables, and the verifier rejects plans that write system tables. The engine changes them only through its API: DDL, `grant` and `revoke`, `set_function_leakproof` and policy proposals. This reference is for operators and contributors who need to know what a database holds.

The on-disk format, these tables included, is not yet stable. Databases are not migrated between versions.

This reference is generated from the tables of a newly created database and from `interlaken_engine::catalog::catalog_table_description`, and a test fails if it falls out of date. Types are storage types. Primary key columns are numbered in key order.

<!-- begin generated: tables -->
## Storage's system tables

### `sys_tables`

One row per table: its ID, name, storage schema version and primary key column positions. Storage's own definitions; the engine's IR types are in `engine_columns`.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `table_id` | `int4` | no | 1 |
| `name` | `text` | no |  |
| `schema_version` | `int4` | no |  |
| `primary_key` | `bytea` | no |  |

Unique index `sys_tables_name` on `name`.

### `sys_columns`

One row per column: its table, position, name, storage type and nullability.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `table_id` | `int4` | no | 1 |
| `column_id` | `int4` | no | 2 |
| `name` | `text` | no |  |
| `type` | `int2` | no |  |
| `nullable` | `bool` | no |  |

### `sys_indexes`

One row per secondary index: its table, name, key columns with their order, whether it is unique, and whether a unique index's NULLs are distinct.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `table_id` | `int4` | no | 1 |
| `index_id` | `int4` | no | 2 |
| `name` | `text` | no |  |
| `columns` | `bytea` | no |  |
| `unique` | `bool` | no |  |
| `nulls_distinct` | `bool` | no |  |

Unique index `sys_indexes_name` on `name`.

### `sys_counters`

Named counters; `object_id` hands out table and index IDs.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `name` | `text` | no | 1 |
| `value` | `int8` | no |  |

## The engine's tables

### `engine_attributes`

The attributes of each entity type, in order: the column holding each, and the entity type it refers to, for references.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `entity` | `text` | no | 1 |
| `position` | `int4` | no | 2 |
| `name` | `text` | no |  |
| `column_id` | `int4` | no |  |
| `refs` | `text` | yes |  |

### `engine_catalog`

One row holding the catalog-wide schema version. Every DDL statement increments it, and every statement reads it, so concurrent DDL and writes conflict, and prepared plans pinned to an older version are rejected.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `id` | `int8` | no | 1 |
| `schema_version` | `int8` | no |  |

### `engine_columns`

The IR type, as type text (for example `int8` or `(option text)`), of each column created through the engine; a column without a row, such as the catalog tables' own, has its storage type's IR namesake.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `table_id` | `int4` | no | 1 |
| `column_id` | `int4` | no | 2 |
| `type` | `text` | no |  |

### `engine_entities`

Entity type declarations: the table holding each type and its key column.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `name` | `text` | no | 1 |
| `table_id` | `int4` | no |  |
| `key_column` | `int4` | no |  |

### `engine_foreign_key_columns`

The column pairs of each foreign key, in order: each referencing column and the referenced column it holds a value of.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `name` | `text` | no | 1 |
| `position` | `int4` | no | 2 |
| `column_id` | `int4` | no |  |
| `referenced_column` | `int4` | no |  |

### `engine_foreign_keys`

Foreign key declarations: the referencing and referenced tables, the unique index whose columns are the referenced columns (NULL for the primary key), the action on delete (`no-action`, `cascade` or `set-null`), and whether it matches `MATCH FULL`.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `name` | `text` | no | 1 |
| `table_id` | `int4` | no |  |
| `referenced_table` | `int4` | no |  |
| `referenced_index` | `text` | yes |  |
| `on_delete` | `text` | no |  |
| `match_full` | `bool` | no |  |

### `engine_functions`

Function properties set by administrators: whether each function is leakproof.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `function_id` | `int4` | no | 1 |
| `leakproof` | `bool` | no |  |

### `engine_memberships`

Membership declarations between entity types: the edges table and its member and group columns, and the closure table and its descendant, ancestor and path-count columns.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `member` | `text` | no | 1 |
| `grp` | `text` | no | 2 |
| `edges` | `int4` | no |  |
| `edge_member` | `int4` | no |  |
| `edge_group` | `int4` | no |  |
| `closure` | `int4` | no |  |
| `closure_descendant` | `int4` | no |  |
| `closure_ancestor` | `int4` | no |  |
| `closure_paths` | `int4` | no |  |

### `engine_namespaces`

Namespaces and their owners (ADR-0042). `public` exists in every database; objects in other namespaces are stored as `namespace.name`.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `name` | `text` | no | 1 |
| `owner_type` | `text` | no |  |
| `owner_key` | `text` | no |  |

### `engine_policy_proposals`

Proposed policy changes: the version each is based on, its language, text and invariants, who proposed it, its state, the version it became when activated and who confirmed it.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `id` | `int8` | no | 1 |
| `base_version` | `int8` | no |  |
| `language` | `text` | no |  |
| `body` | `text` | no |  |
| `invariants` | `text` | no |  |
| `proposer` | `text` | no |  |
| `state` | `text` | no |  |
| `activated_version` | `int8` | yes |  |
| `confirmed_by` | `text` | yes |  |

### `engine_policy_set`

One row holding the active policy set: its version, language and text.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `id` | `int8` | no | 1 |
| `version` | `int8` | no |  |
| `language` | `text` | no |  |
| `body` | `text` | no |  |

### `engine_policy_settings`

One row holding the policy settings: the verification mode and the Cedar type of `context.app`.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `id` | `int8` | no | 1 |
| `mode` | `text` | no |  |
| `app_context` | `text` | no |  |

### `engine_policy_verifications`

The result of each deployment gate a verifier ran on a proposal: whether it passed, with details.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `proposal` | `int8` | no | 1 |
| `verifier` | `text` | no | 2 |
| `gate` | `text` | no | 3 |
| `passed` | `bool` | no |  |
| `detail` | `text` | no |  |

### `engine_principal_types`

Principal types that no table backs, such as the first administrator's: policies can name them, and their principals have no attributes or ancestors.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `name` | `text` | no | 1 |

### `engine_privileges`

Privileges (`admin`, `read-policies`) granted to principals.

| Column | Type | Nullable | Primary key |
| --- | --- | --- | --- |
| `principal_type` | `text` | no | 1 |
| `principal_key` | `text` | no | 2 |
| `privilege` | `text` | no | 3 |

<!-- end generated: tables -->
