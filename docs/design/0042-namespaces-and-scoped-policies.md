---
id: 0042
title: Namespaces and Scoped Policies
status: proposal  # proposal | accepted | implemented | superseded
tags: [catalog, engine, security, ir]
feature-flag:
created: 2026-11-25
accepted:
implemented:
spec-sections: []
superseded-by:
---

# ADR-0042: Namespaces and Scoped Policies

## Status

Proposal.

## Summary

A database has one flat set of names and one active policy set. That suits one application per database. It doesn't suit a database shared by several applications, or by one application made of independently developed parts:

- **Names collide:** every table, index, entity type and foreign key name is global, so each part has to prefix its names by convention.
- **Policies are all-or-nothing:** every part's policies have to be merged into one document and replaced together. A change to one part re-verifies all of them, and nothing stops one part's policies from granting access to another part's tables.
- **No ownership:** `admin` is the only privilege for DDL and policy changes, so whoever may change one part's schema may change everything.
- **Principals must be backed by tables:** Cedar can only name principals whose type is a declared entity type. A principal like `Admin::"root"`, or a service principal no table describes, is refused with an invalid-request error (backlog `undeclared-principal-types`).

This ADR decides:

1. **One level of namespaces.** Every table, index, entity type, membership and foreign key belongs to a namespace and is named `ns.name`. Existing objects are in the namespace `public`.
2. **Resolution through a search path:** unqualified names resolve through the session's `search_path`, `["public"]` by default.
3. **Owners:** each namespace has an owner principal, who can change its schema and policies without `admin`.
4. **A policy set per namespace, plus a database-wide baseline.** A namespace's policies can only grant access to its own resources; the baseline's `forbid` rules apply everywhere and no namespace can override them.
5. **Principal types:** types that no table backs can be declared for Cedar's schema, and requests from undeclared types are denied rather than invalid.

## Context

### Names today

- Storage keeps table names in `sys_tables`, unique through an index; index names are unique across the database. Names are checked only for emptiness and the reserved `sys_` prefix.
- The engine keeps entity types, memberships and foreign keys in `engine_` tables keyed by global names, and reserves the `engine_` prefix.
- The IR's `TableDef.name` is already a `QualifiedName`, and `table_by_name` takes one. The text syntax reads dotted symbols as qualified names (`d.title`), and a table in `get` may have any number of parts. Loading a stored name splits it on `.`.
- Entity types are keyed by a single `Ident`.
- Resolution matches names exactly; there is no search path.

### Policies today

- `engine_policy_set` holds one row: version, language and body. Proposals record the version they were based on and go stale when it moves.
- The provider is built from the whole document over every declared entity type. Cedar's schema lists every type, unqualified, with one action line that lets any type be principal or resource. Cedar namespaces (`Ns::Type`) aren't used.
- The combining rule is permit-unless-forbid: a request is allowed if some permit applies and no forbid does.
- Policies see a resource's entity type, key and attributes.

### Privileges today

`admin` and `read-policies` are the only privileges. Every DDL, policy and administrative operation needs `admin`, and admins are still governed by row policies (ADR-0031).

### Undeclared principals

The generated Cedar schema lists only declared entity types as principals. A request from any other type fails Cedar's request validation, which surfaces as `42501` "secured plan is invalid: Cedar partial request: principal type `Admin` is not valid". The RLS sublanguage has no schema and accepts `(is principal Admin)`.

## Decision

### 1. Namespaces

- **Catalog:** a new `engine_namespaces(name, owner_type, owner_key)` table. `public` exists in every database, owned by the database's creating principal.
- **Names:** tables, indexes, entity types, memberships and foreign keys are stored with their namespace, as `ns.name` in their existing name columns. Name uniqueness is per namespace, which the existing unique indexes give for free.
  - Index and foreign key names are scoped to their table's namespace.
  - Entity types become qualified: the catalog key moves from `Ident` to `QualifiedName`.
- **Reserved names:** `sys_` and `engine_` stay reserved, and outside any namespace. Namespace names follow identifier rules, and `public` can't be dropped.
- **Operations:**
  - `create_namespace(name, owner)` (needs `admin`);
  - `drop_namespace(name, cascade)` (needs `admin`; without `cascade`, only an empty namespace);
  - `set_namespace_owner(name, owner)` (needs `admin`).
- **Cascade** drops, in dependency order:
  - the namespace's foreign keys, and those in other namespaces that refer to its tables;
  - its memberships, entity types, indexes and tables;
  - its policy set;
  - its files (ADR-0044).

  It runs in one transaction, like any DDL.
- **Existing databases** need no migration: unqualified stored names are read as `public.name`.

### 2. Resolution

- `Session::set_search_path(["app", "public"])` sets the order unqualified names are tried in. The default is `["public"]`.
- The resolver looks up a one-part table or function name in each namespace on the path; a two-part name is exact. Built-in functions stay unqualified and are found first.
- Column references keep at most one qualifier, the table's alias or last name part, so `(get app.todos :as t)` columns are `t.title`.
- Entity type names in DDL resolve the same way. Policies name types fully (section 4).
- Prepared plans pin the resolved tables, so a later search path change doesn't change a prepared plan.

### 3. Owners

A namespace's owner may, within that namespace:

- run DDL on its tables, indexes, entity types, memberships and foreign keys;
- submit, verify and activate its policy set (section 4);
- grant ownership on to another principal.

DDL that crosses namespaces needs ownership of both: a foreign key from `app.orders` to `crm.customers`, or a membership between their types. `admin` covers every namespace.

Owners are governed by row policies like everyone else (ADR-0031). Owning a namespace grants schema and policy changes, not data access.

### 4. Policy sets per namespace

`engine_policy_set` becomes `engine_policy_sets(scope, version, language, body)`, keyed by scope: the empty scope is the database-wide **baseline**, and every other row is a namespace's set.

- **Effective policies:** a request on a resource in namespace `N` is decided by the baseline together with `N`'s set, combined permit-unless-forbid as today. A forbid in either set denies, so the baseline's forbids can't be overridden.
- **Confinement:** a namespace's policies can only concern its own resources. Validation rejects a policy in `N`'s set whose resource constraint could match a type outside `N`. Cedar's `resource is N::Type` or `resource in N::Type::"..."` satisfies it; an unconstrained `resource` doesn't. Principals and their attributes may be of any namespace's types: users in `identity`, documents in `app`.
- **The baseline** applies to every namespace. It typically holds forbids (no access outside business hours, no access to a quarantined type) and grants for administrators.
- **Cedar naming:** an entity type `T` in namespace `N` is the Cedar type `N::T`. Types in `public` are unqualified, so existing policies keep working unchanged. The generated schema puts each namespace's types in a Cedar `namespace` block, and the action line covers them all.
- **The RLS sublanguage** writes qualified types the same way it writes names: `(is resource app.Todo)`.
- **Proposals** name their scope. Verification and activation are per scope, and staleness is per scope: a change to `app`'s set doesn't make `crm`'s proposals stale. A baseline proposal's impact analysis covers every namespace, since it affects them all.
- **Language:** a database's sets share one language, chosen by the baseline's.
- **Providers** are built per namespace, from the baseline plus that namespace's set, and cached by schema version and both sets' versions.
- **Policies see namespaces** through the type: `resource is app::Todo` in Cedar. A query joining tables from two namespaces filters each table with its own namespace's effective policies.

### 5. Principal types without tables

- `declare_principal_type(name)` adds a principal type with no table and no attributes, such as `public.Admin` or `svc.Service`. It appears in the Cedar schema as `entity Admin;`, so policies can name it: `permit (principal is Admin, action, resource);`. Entities of it have no attributes and no ancestors.
- `Engine::create` declares the creating principal's type, if no table backs it, as a principal type in `public`.
- A request from a principal whose type isn't declared at all is **denied**, not invalid. The provider checks the principal type before building Cedar's request, so the error becomes a plain denial. RLS already behaves this way.

This closes backlog `undeclared-principal-types`.

## Alternatives considered

- **Prefixes by convention.** No engine work, but no ownership, no scoped policies, and nothing enforces the convention.
- **One database per application.** Full isolation, but no transactions, joins or shared principals across applications, and each database runs its own cluster.
- **Nested namespaces.** More general, but one level covers per-application isolation, and every extra level complicates resolution and policy scoping.
- **Namespace sets that may override the baseline** (explicit allow over a baseline forbid). It needs a combining rule beyond permit-unless-forbid, and makes the baseline advisory.
- **Unconfined namespace policies.** Simpler to validate, but a namespace could grant itself access to another's tables, which defeats the separation.

## Testing

- **Resolution:** search path order, exact two-part names, ambiguous names, prepared plans unaffected by later path changes.
- **Catalog:** the same names in two namespaces; cascade drops everything in order, including foreign keys from other namespaces; an existing database opens with its objects in `public`.
- **Ownership:** an owner's DDL and policy changes are allowed in its namespace and refused elsewhere; cross-namespace DDL needs both owners.
- **Policies:**
  - the security oracle tests (ADR-0021) across two namespaces with a baseline;
  - confinement rejects a policy that could match another namespace's types;
  - a baseline forbid denies regardless of a namespace's permits;
  - per-scope staleness.
- **Principal types:** a declared principal type is granted by Cedar; an undeclared type is denied, not invalid.
- **The impact analysis** of a baseline change reports per namespace.

## Implementation Phases

### Phase 1: principal types

- `declare_principal_type`, the creating principal's type, and denial of undeclared types.

### Phase 2: namespaces and resolution

- `engine_namespaces`, qualified names in the catalog and the IR, the search path, create and drop with cascade.

### Phase 3: owners

- Ownership checks on DDL and policy operations; cross-namespace DDL.

### Phase 4: scoped policy sets

- `engine_policy_sets`, confinement, per-namespace providers, Cedar namespaces in the schema, proposals per scope.
- The book: namespaces, ownership and scoped policies.

## Consequences

### Positive

- Applications or parts of one can share a database without colliding, each owning its schema and policies.
- A namespace's policy changes verify and activate alone, and can't reach other namespaces.
- The baseline gives one place for rules every namespace obeys.
- Administrators and services can be principals without backing tables.

### Negative

- Entity types become qualified, a change through the catalog, the Cedar schema and the policy tests.
- Providers multiply: one per namespace in use, each validated against the whole schema.
- Confinement rejects some policies that are harmless today (an unconstrained `resource` in a namespace set must name its types).
- Cross-namespace foreign keys tie namespaces together, so dropping one cascades into others.

## Open Questions

- Should a namespace be able to expose some of its tables to other namespaces' policies (shared reference data), or must the baseline grant that?
- Should owners be groups (any member of a group entity) as well as single principals?

## Future Work

- Per-namespace quotas on rows and bytes, beside the file quotas of ADR-0044.
- Namespace-level privileges finer than ownership (read-only schema access, policy review without activation).
- SQL's `CREATE SCHEMA` and `SET search_path`, lowered onto these operations.

## References

- [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md): names and resolution.
- [ADR-0017](0017-entities-and-hierarchy-closure-table.md): entity types.
- [ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md): the Cedar provider and its schema.
- [ADR-0030](0030-policy-proposals-and-distributed-verification.md): proposals and verification.
- [ADR-0031](0031-engine-sessions-and-storage-backed-catalog.md): privileges and the stored catalog.
- Backlog `undeclared-principal-types`.
