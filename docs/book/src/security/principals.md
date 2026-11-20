# Principals and request context

## The security context

A session runs as one principal, given as a `SecurityContext` when the session starts:

```rust,ignore
use interlaken_ir::security::{EntityUid, SecValue, SecurityContext};

let mut context = SecurityContext::new(EntityUid::long("User", 2));
context.standard.auth_method = "mfa".into();
context.standard.ip = "203.0.113.7".into();
context.application = vec![("tenant".into(), SecValue::Str("acme".into()))];
let mut session = engine.session(context);
```

The application authenticates the principal and builds the context. The engine trusts it, and client IR can't change it. A `SecurityContext` has:

- **`principal`:** the `EntityUid` of an entity: `EntityUid::long(type, key)` for integer keys, `EntityUid::str(type, key)` for text keys. Policies refer to it as `principal`.
- **`standard`:** request facts policies can read as `context.time` (microseconds since the Unix epoch), `context.ip`, `context.auth_method` and `context.session_id`. They default to zero and empty strings.
- **`application`:** application-defined fields under `context.app`, such as `context.app.tenant`. For Cedar policies, their type must be declared in the policy settings (see [Changing policies](proposals.md#policy-settings)).

`Session::context()` returns the session's context. A different principal needs a different session.

## The principal's entity

When a transaction starts, the engine loads the principal from its entity table, through the transaction:
- its attributes, from its row;
- every group it belongs to, directly or transitively, from the closure tables.

Policies read them as `principal.dept` and test them with `principal in Group::"7"`. Loading registers the reads in the transaction, so authorization sees the same snapshot as the statement (see [Authorization is transactional](model.md#authorization-is-transactional)).

A principal needn't have a row. Its attributes are then absent, so conditions reading them don't hold, and it belongs to no groups. A principal whose entity type isn't declared can't be described by Cedar policies at all (see [Governed tables](model.md#governed-tables)).

Policies can also read other entities through references and by name: `principal.manager.dept`, or `User::"7".dept` in Cedar. The engine loads those entities the same way, so a change to a manager's department conflicts with a transaction that relied on it, as a change to the principal's own would.

## Partial evaluation and the residual cache

The principal, its attributes and groups, and the context are all known before a statement runs. For each governed table, the engine evaluates the policies with those values and keeps only what depends on the row, the residual. This is why a policy like `principal in Group::"7"` costs nothing per row: it is decided once, before the query reads anything.

Residuals are cached, keyed by everything they can depend on:

- the principal, the action and the resource type;
- the policy version;
- a fingerprint of the principal's attributes and memberships, and of every other entity the policies read;
- the values of the context fields the policies read.

A changed policy, attribute or membership therefore misses the cache, and stale entries are never used. Residuals that depend on fast-changing context, `time` or `ip`, aren't cached. `Engine::residual_cache_stats()` reports the cache's hits and misses. `EngineOptions::residual_cache_capacity` sizes it (see [Configuration](../operations/configuration.md)).
