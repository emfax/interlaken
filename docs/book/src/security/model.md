# The security model

Every statement runs as a *principal*, and the engine itself enforces which rows and columns that principal may read and write. Security isn't a layer an application must remember to call. Policies are compiled into each statement's plan before it runs, so every query, join, subquery, aggregate and write sees only what the principal may see.

## Governed tables

A table is *governed* once it backs an entity type (see [Entities and memberships](../ddl/entities.md)):

- **Ungoverned tables** are open to every principal: tables that back no entity type, such as edges and closure tables and lookup tables.
- **Governed tables** are closed by default. Each principal may read and write exactly the rows that policies allow.

**Declaring an entity type closes its table at once, for everyone, the creating administrator included.** Until a policy set that permits access is active, every read of the table returns no rows, and every write is refused with `42501`. Plan the order: create tables, declare entity types, activate policies, then load data. Alternatively, load the data before declaring entity types.

A principal whose type isn't a declared entity type, such as the `Admin::"root"` principal in this book's examples, has a further problem. Cedar policies can't name it, and until an RLS policy set is active, its queries on governed tables fail with `42501` and the message "secured plan is invalid … principal type `Admin` is not valid". So prefer an administrator of a declared entity type, such as a `User`. Pass that principal to `Engine::create` as the first administrator, and grant it access in the policies like anyone else. Allowing undeclared principal types is backlog item `undeclared-principal-types`.

## Policies

A policy *permits* or *forbids* an action on resources of one entity type, when a condition holds. The four actions are:

| Action | Governs |
| --- | --- |
| `read` | which rows a `get` returns: every read, including the rows an update or delete starts from |
| `insert` | which new rows an insert may write |
| `update` | which rows an update may change, and what they may become |
| `delete` | which rows a delete may remove |

The decision for a principal, an action and a resource row is:

- **allowed** if at least one permit's condition holds and no forbid's does;
- **denied** otherwise. With no applicable permit, the answer is no.

A condition that raises an error, such as reading an absent attribute, doesn't hold. An erroring permit grants nothing, and an erroring forbid forbids nothing, which is why forbids should be written so they can't fail. The verification gates warn about forbids that can (see [Changing policies](proposals.md)).

Policies are written in [Cedar](cedar.md) or in the [RLS sublanguage](rls.md). Both compile to the same thing. *Column policies* additionally decide whether a column may be read, and can show a masked value instead (see [Column policies and masks](masks.md)).

A policy set is changed only through a verified proposal, which an administrator activates. The active set is versioned, and every statement uses the version current in its transaction.

## How policies become plans

Before a statement runs, the engine works out what the policies mean for this principal:

1. **Load the principal:** its attributes and the groups it belongs to, from the transaction's snapshot.
2. **Partially evaluate:** for each governed table the statement touches, evaluate the policies with everything known before the query runs. That covers the principal, its attributes and memberships, and the request context. Only the parts that depend on the row are left. The result is a *residual*: always true, always false, or a condition over the row.
3. **Lower:** add the residual to the plan:
   - as a `PolicyFilter` above each read of the table, a security barrier;
   - as a filter on the rows an update or delete starts from, as Postgres's `USING` does;
   - as a `WriteCheck` on every row an insert or update writes, as Postgres's `WITH CHECK` does.

   Row-dependent column policies become masks or column denials.
4. **Optimize:** the optimizer may move a predicate below a barrier only if it is *leakproof*: it can't raise an error or otherwise reveal anything about a row it is evaluated on. Otherwise a query could probe rows it may not see through errors (`(/ 1 (- d.secret 42))`). Comparisons are leakproof, so filters on the principal's own conditions still use indexes. Built-in functions aren't, until an administrator marks them (see [Privileges](privileges.md)).

For the policy `permit read on Doc when resource.owner == principal` and principal `User::"2"`, the residual is `owner = 2`. The plan reads `docs` through `(policy-filter ... (= owner 2))`, and with an index on `owner` it reads only those rows. A residual that is always true adds nothing, and one that is always false reads nothing.

Policy conditions can follow references and test memberships: `resource.owner.dept == principal.dept`, or `principal in Group::"7"`. These become joins to the referenced tables and lookups in closure tables, which the optimizer plans like any other.

## Authorization is transactional

Everything a decision depends on is read through the statement's transaction:

- the principal's attributes and memberships;
- the attributes of entities the policies follow;
- the active policy set.

A transaction sees a consistent snapshot of all of them, its own changes included. So a transaction that adds a user to a group is authorized with the new membership in its next statement. Concurrent changes are caught at commit:

- **Policy changes:** a transaction that wrote anything fails with `40001` if the policy set changed concurrently, at every isolation level.
- **Membership and attribute changes:** under SERIALIZABLE, a transaction that wrote anything fails with `40001` if a membership or attribute it was authorized by changed concurrently. A revocation then can't slip past a writer that relied on the old grant. Under READ COMMITTED and REPEATABLE READ, such a transaction commits on the authorization it started with.

Residuals are cached by policy version, principal and the versions of what they read. Most statements skip partial evaluation entirely.

## What principals see

| Situation | What happens |
| --- | --- |
| a row the read policies deny | it isn't there: queries, joins, subqueries and aggregates never see it |
| a column the column policies deny for some rows, without a mask | the statement fails with `42501`, "permission denied for column ..." |
| the same with a mask | the column shows the masked value on those rows |
| an update or delete of a row the action's policies deny | that row isn't changed, silently, as in Postgres |
| an insert or update writing a row the policies deny | the statement fails with `42501`, "permission denied for table ...", naming the table but never a row's values |
| `EXPLAIN` without the `read-policies` privilege | policy logic shows as `<policy>` (see [EXPLAIN and policy visibility](explain.md)) |

Errors never reveal hidden rows: the error for a denied write names only the table.
