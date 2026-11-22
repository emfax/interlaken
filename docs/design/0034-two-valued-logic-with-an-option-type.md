---
id: 0034
title: Two-Valued Logic with an Explicit Option Type
status: accepted  # proposal | accepted | implemented | superseded
tags: [ir, types, executor, sql]
feature-flag:
created: 2026-11-21
accepted: 2026-11-21
implemented:
spec-sections: ["ir-design.md#type-system", "ir-design.md#expressions"]
superseded-by:
---

# ADR-0034: Two-Valued Logic with an Explicit Option Type

## Status

Accepted. Phases 1 and 2, the option type and two-valued expressions, are built; phases 3 to 6 aren't. Decomposition was considered as the alternative and rejected; [Alternative considered: decomposition](#alternative-considered-decomposition) records why.

## Summary

Replace the `nullable` flag on every IR type, and SQL's NULL with its three-valued logic, with an explicit type constructor, `(option T)`. Its values are `(some v)` and `none`. `none` is an ordinary value:
- it equals itself;
- it has a fixed place in the sort order;
- operators don't extend over options implicitly. A plan says when an absent value propagates (`lift`), defaults (`coalesce`) or is an error (`unwrap`).

Every predicate is two-valued, and the algebra's laws hold again: `x = x` is always true and `p OR NOT p` always holds. The SQL front end keeps full SQL by lowering NULL into explicit option operations, so NULL becomes SQL syntax rather than IR semantics. Arrow's validity bitmaps and the storage row format already represent `none` exactly as they represent NULL, so the physical layers barely change.

## Context

### What NULL is today

ADR-0010 fixed "explicit nullability, three-valued logic for SQL predicates", and ADR-0024 implemented Postgres's NULL semantics in the executor. Every `Type` carries `nullable: bool`, written `(not-null T)` or `T` in IR text. The non-test source refers to nullability 161 times across 30 files. NULL arises from:
- nullable stored columns, and inserts that omit a column (there are no defaults);
- the padded side of `left`, `right` and `full` joins, and `ApplyKind::Left` and `Single` (scalar subqueries with no rows);
- `sum`, `avg`, `min` and `max` over an empty group;
- `case` without `else`, `nullif`, and the `null` literal;
- column masks whose mask is NULL (ADR-0032).

### What is wrong with it

- **NULL is not a value.** A relation is a set of tuples of values from domains, and NULL belongs to no domain. Codd's later two-mark proposal and Date and Darwen's *Third Manifesto* both reject SQL's single NULL.
- **Three-valued logic breaks the algebra's laws.** `x = x` can be unknown. `p OR NOT p` isn't a tautology. A `NOT IN` subquery containing a NULL returns no rows, so `x NOT IN (s)` and `NOT EXISTS (… s.y = x)` differ.
- **SQL is inconsistent about it.** `GROUP BY`, `DISTINCT` and set operations treat NULLs as equal, but `=` doesn't. Unique indexes admit any number of NULLs. `SUM` of no rows is NULL, not 0. `COUNT(*)` and `COUNT(col)` differ.
- **It constrains the optimizer.** Many rewrites that are valid in two-valued logic are not valid in three: `NOT (a = b)` to `a <> b`, `NOT IN` to an anti-join, predicate implication for index selection, and simplifications across `case`. The optimizer either checks nullability before each one (ADR-0014) or forgoes it.
- **Two logics meet at the security boundary.** The security sublanguage has no NULL (ADR-0016), matching Cedar. A nullable column becomes an *optional attribute* at the boundary, which is why Cedar policies need `has` for nullable columns. A mask of NULL makes a column's values depend on the principal in a way the type system can't describe.
- **It leaks into types.** Backlog `array-element-nullability`: an array's element nullability makes `(array (not-null text))` and `(array text)` different types with no conversion between them.

### Why decide now

The SQL front end doesn't exist yet (`mvp-completion.md`, *Beyond the MVP*). No client depends on SQL semantics passing straight through the IR, so the IR can change its semantics now and the front end can be written against the new ones. Once the front end and its differential tests against Postgres exist, this change would touch all of them too.

## Decision

### Types

- **`(option T)`** is a type constructor. Its values are `(some v)` for every value `v` of `T`, and `none`.
- **Plain types never hold `none`.** `int8` is always an integer. `(not-null T)` and the `nullable` flag are removed: the old `T` becomes `(option T)` and the old `(not-null T)` becomes `T`.
- **No nested options.** `(option (option T))` is rejected with `42804`. A single validity bitmap can't represent two levels of absence, and no front end needs them.
- **Composite types** take options in their parts: `(array (option text))` is an array whose elements may be absent, distinct from `(option (array text))`, an array that may be absent. Implicit conversions widen `T` to `(option T)` everywhere, including inside arrays and records, which resolves `array-element-nullability`.

### Values and expressions

| Form | Type | Meaning |
| --- | --- | --- |
| `none`, `(none T)` | `(option T)` | absence; the bare form takes its type from context, as `null` does today |
| `(some e)` | `(option T)` for `e: T` | presence; usually implicit, by widening |
| `(is-some e)`, `(is-none e)` | `bool` | test |
| `(coalesce e1 ... en)` | `T` if `en: T`, else `(option T)` | the first present value |
| `(unwrap e)` | `T` | the value, or error `22004` if absent |
| `(lift F e1 ... en)` | `(option R)` for `F: (T1 ... Tn) -> R` | `none` if any argument is `none`, else `(some (F v1 ... vn))` |

- **Equality and order are total.** `none = none` is true, and `(some a) = (some b)` iff `a = b`. Equality on options is exactly SQL's `IS NOT DISTINCT FROM`. `none` sorts last in ascending order and first in descending, matching Postgres's default NULL placement, and sort keys take `:none first|last`.
- **Nothing lifts implicitly.** Arithmetic, comparison and function calls are defined on plain types. Applying one to an option is a type error (`42804`) unless the plan says `lift`. `lift` is today's strict NULL propagation, made explicit and visible in `EXPLAIN`.
- **Predicates are `bool`.** The conditions of `filter`, `join`, `case` and `aggregate :filter`, and policy residuals, must have type `bool`, never `(option bool)`.
- **No three-valued `and`/`or` in the IR's core.** `and`, `or` and `not` take `bool`. The SQL front end lowers SQL's Kleene logic with the functions `and3`, `or3` and `not3` on `(option bool)`, which are ordinary leakproof catalog functions.

### Relational operators

| Operator | Today | With options |
| --- | --- | --- |
| `left`/`right`/`full` join, `ApplyKind::Left` | padded columns are nullable, padded with NULL | padded columns are `(option T)`, padded with `none` |
| `ApplyKind::Single` (scalar subquery) | NULL when empty | `(option T)`, `none` when empty |
| `count` | `int8` | unchanged |
| `sum` | NULL over an empty group | `T`, the identity (0) over an empty group; the SQL front end restores SQL's NULL with `count` |
| `min`, `max`, `avg` | NULL over an empty group | `(option T)`, `none` over an empty group |
| aggregate arguments | NULLs are skipped | `none`s are skipped: an aggregate over `(option T)` aggregates the present values |
| `distinct`, `group by`, set operations | NULLs are equal | unchanged: `none` equals `none` |
| `insert` omitting a column | NULL | allowed only for `(option T)` columns, giving `none` |

Bag semantics (ADR-0010) is unchanged. This ADR is about missing values, not duplicates.

### Storage and execution

- **Representation is unchanged.** An `(option T)` column is a nullable Arrow array, and `none` is a cleared validity bit. Stored rows keep the null bitmap (ADR-0003), and keys keep the tag that orders absent values (ADR-0003, ADR-0023).
- **Kernels.** `lift` uses today's strict kernels unchanged. `and3`, `or3` and `not3` are today's three-valued `AND`, `OR` and `NOT`. Comparisons on options become total-order kernels, which already exist for `IS [NOT] DISTINCT FROM` and sorting.
- **Unique indexes.** Under option equality, two `none` keys are equal. Postgres treats NULLs in a unique key as distinct unless the index says `NULLS NOT DISTINCT`. `IndexSpec` gains `nones_distinct: bool`, default `true`, so today's behaviour and SQL's are kept, and an index can opt into option equality.
- **Storage constraints.** NOT NULL constraints are derived from types: a plain-typed column rejects absent values, as a NOT NULL column does today.

### The SQL front end

The front end lowers NULL to options at the boundary, so full SQL keeps its meaning:

| SQL | IR |
| --- | --- |
| a nullable column `c` of type `T` | `c: (option T)` |
| `NULL` | `none` |
| `a = b`, `a + b`, `f(a)` with option operands | `(lift = a b)`, `(lift + a b)`, `(lift f a)` for strict `f` |
| `p AND q`, `p OR q`, `NOT p` on `(option bool)` | `(and3 p q)`, `(or3 p q)`, `(not3 p)` |
| `WHERE p`, `ON p`, `HAVING p`, `CASE WHEN p` | `(coalesce p false)` |
| `a IS NULL`, `a IS NOT NULL` | `(is-none a)`, `(is-some a)` |
| `a IS [NOT] DISTINCT FROM b` | `(not (= a b))`, `(= a b)` |
| `x NOT IN (subquery)` | an anti-join on `(lift = x y)`, with SQL's NULL rule spelled out: no rows if the subquery has a NULL `y`, unknown if `x` is NULL |
| `SUM(x)` | `(case (= (agg count x) 0) none (some (agg sum x)))` |
| `COALESCE`, `NULLIF`, `CASE` without `ELSE` | `coalesce`; `case` returning `(option T)`; `case` with `:else none` |

The optimizer simplifies the common shapes away. `(coalesce (lift = c (param 1 T)) false)` on a plain column is `(= c (param 1 T))`, which is an index key. On an option column it is `(and (is-some c) (= (unwrap c) (param 1 T)))`, and also an index key. SQL that never touches a nullable column lowers to plans with no option operations at all.

### Security

- **The sublanguage is unchanged.** It already has no NULL. An `(option T)` column maps to an optional entity attribute (`has`), as a nullable column does now, but the mapping now follows from the type.
- **Masks have the column's type.** A mask of `none` is allowed only on `(option T)` columns. Plain columns are masked with a value of their own type (`redact`, `mask_last4`) or denied. A query's result types then never depend on who runs it.
- **Leakproofness.** `is-some`, `is-none`, `coalesce`, `lift` of leakproof functions, and option equality are leakproof. `unwrap` can raise and isn't.

### Clients and parameters

`Datum::Null` remains the client representation of `none` until a rename is worth the churn. Parameters declare `(param N (option T))`, and `check_params` accepts `Datum::Null` only for option parameters, as it does today for nullable ones.

## Implementation Phases

- [x] **Phase 1: The option type.** `DataType::Option` replaces `Type::nullable`; text syntax, resolver, verifier, typing and the interpreter use it; widening conversions; no semantic change yet, because `nullable` and non-nested `option` are isomorphic. Resolves `array-element-nullability`.
  - *As implemented:*
    - **Representation.** `Type` keeps a flag, renamed from `nullable` to `optional`, instead of gaining a `DataType::Option` variant. Options don't nest, so a flag represents `T` and `(option T)` exactly and makes a nested option impossible to construct, rather than something the verifier must reject. Code that matches on `ty.data` doesn't have to look through a wrapper. The constructors are `DataType::plain` and `DataType::option`, and `Type::widens_to` defines the widening.
    - **Text.** A bare type is plain and `(option T)` is optional. `(option (option T))` and the old `(not-null T)` are parse errors, the latter with a hint. Tests, golden files and the book were converted mechanically: the old `T` became `(option T)` and the old `(not-null T)` became `T`.
    - **Widening.** `typing::implicit`, and so `assignment` and `explicit`, accept plain to optional elements and fields inside arrays and records. The resolver inserts a cast, which is the identity in the interpreter and the executor, because Arrow list elements and struct fields are always nullable there.
    - **Messages** say "whose type isn't optional" where they said "NOT NULL column". Storage keeps its NOT NULL constraints, derived from the types as before.
    - **Stored catalogs.** `engine_columns` holds type text, and there is no catalog format version. A database created before this change loads with the right top-level optionality, because storage's column nullability decides it. But stored `(not-null T)` text no longer parses, and an old `(array T)` now reads as having plain elements. The project hasn't been released, so no migration was written.
  - *Testing:* `typing::tests::composite_widening`; `array_elements_widen_to_options` in `crates/engine/tests/session.rs` writes arrays with and without NULL elements to an `(array (option text))` column through a session, and is refused when writing optional elements to an `(array text)` column.
- [x] **Phase 2: Two-valued expressions.** Total equality and order on options; `lift`, `coalesce`, `unwrap`, `is-some`, `is-none`; `and3`, `or3`, `not3` as catalog functions; predicates must be `bool`; implicit lifting removed from the resolver. The interpreter defines the semantics first, then the executor's kernels follow, with the differential tests rewritten to the new semantics.
  - *As implemented:*
    - **IR text keeps SQL shorthand** (the first open question, answered yes). `(= u.age 30)` on an optional `age` still means SQL's lifted comparison, and `and`/`or`/`not` over optional booleans are three-valued. `logic::normalize` rewrites the shorthand into explicit forms after resolution, after the optimizer, and, through `resolve`, after security lowering. It wraps strict operators and calls with optional operands in `lift`, turns `and`, `or` and `not` over optional booleans into `and3`, `or3` and `not3`, and makes every predicate plain by pushing `coalesce` with `false` through `and3`, `or3` and `not3`. The result is `and`/`or` of atoms, each optional atom written `(coalesce (lift (cmp a b)) false)`. The shorthand always means SQL and normalized plans hold none, so the pass is idempotent and EXPLAIN output can be pasted back in. Rules that rewrite plans (decorrelation, the quantified rewrite) keep writing shorthand and rely on the final pass. The pass tracks which bindings an outer join or apply padded, since `verify::column_types` records a column's type where it is introduced.
    - **`lift` wraps one operator or call**, `(lift (= a b))`, rather than taking the operator as an argument, `(lift = a b)`. Its operands are the inner operator's.
    - **Total equality is `is-not-distinct-from`**, not `=`. Every strict operator with an optional operand must be lifted, comparisons included, so `=` never has two meanings depending on whether a plan is resolved. Ordering comparisons on options likewise need `lift`; sorting is already total, with `none` last ascending.
    - **The verifier** rejects a strict operator or function with an optional operand outside `lift` (`42804`-class type error), and a predicate or `and`/`or`/`not` operand of type `(option bool)`. It reports type mismatches before missing lifts. `column_types` and `expr_type` don't enforce this, since they also type shorthand.
    - **Evaluation barely changed.** `lift` compiles to its inner operator, whose kernels already propagate NULL. `and3` and `or3` compile to the connective kernels and short-circuit in the interpreter too: the kernel differential test found the interpreter evaluating both arguments of `or3`, which raised an error `or` would have avoided. `not3` is the `not` kernel.
    - **`unwrap` isn't a new form:** `assert_present` already is it, failing with `22004`. `is-some` and `is-none` are `is-not-null` and `is-null`.
    - **Plans keep their index use and pushdown** (phase 4's simplification moved here). `logic::comparison` recognises `(coalesce (lift (cmp a b)) false)` as `cmp a b` for index access (`exec::access`), hash join keys (`exec::physical`) and decorrelation's correlated equalities. The quantified rewrite sees through `lift` in its aggregate arguments. Both forms are true exactly when both operands are present and the comparison holds, as the filter meant before.
- [ ] **Phase 3: Operators and aggregates.** Outer joins and `Single` apply produce options; `sum` returns its identity; `min`, `max` and `avg` return options; inserts may omit only option columns.
- [ ] **Phase 4: Optimizer.** Rewrites that are now valid in two-valued logic; simplification of lowered SQL patterns back to index keys; property tests that lowered and hand-written forms plan alike.
- [ ] **Phase 5: Storage and security.** `IndexSpec::nones_distinct`; typed masks; the Cedar schema from option types.
- [ ] **Phase 6: SQL lowering.** Written with the SQL front end: the lowering table above, and differential runs against Postgres through the lowering.

## Consequences

### Positive

- The IR's logic is two-valued and its laws hold, so every rewrite valid in relational algebra is valid in the optimizer.
- Absence is visible: every place a value can be missing has an option type, and every place absence propagates is a `lift` in the plan.
- The query half and the policy half of the system have the same model of absence.
- Full SQL stays available, with NULL as front-end syntax, and the differential testing against Postgres planned for the SQL front end still applies.
- GraphQL's nullable fields map directly to options, with no three-valued logic to explain.
- Storage and Arrow representations don't change.

### Negative

- A milestone-sized change: the expression semantics of ADR-0024, a large part of the interpreter and kernels, the differential tests, and every chapter of the book that discusses NULL.
- Lowered SQL predicates are bigger, and the optimizer must see through them to keep using indexes. A missed simplification is a performance bug, not a wrong answer.
- Hand-written IR gets more verbose where values really are optional: `(lift + a b)` instead of `(+ a b)`.
- Unique index semantics need a flag to keep SQL's behaviour.
- NULL masks on plain columns are gone; applications that masked with NULL must use typed masks or option columns.

## Open Questions

- Should IR text offer implicit lifting as sugar, desugared by the resolver into visible `lift`s, to keep hand-written IR short?
- Should the default `none` placement follow Postgres (last ascending), or should `none` sort first, as `Option` does in Rust?
- Should `sum` over an empty group be the identity in the IR, as proposed, or return an option to match SQL directly?
- Should `Datum::Null` be renamed `Datum::None`, and when?

## Alternative considered: decomposition

The other way to remove NULL keeps no option type either. Every relation holds only values, as Date and Darwen propose:
- an optional attribute moves to its own table keyed by its entity, and a missing row means the value is unknown;
- `nest` or semi- and anti-joins replace outer joins;
- keyless `min`, `max` and `avg` over empty input return no row or take an explicit default;
- an empty scalar subquery is an error.

| | Option type (this ADR) | Decomposition |
| --- | --- | --- |
| Missing values in stored rows | `(option T)` columns | none: optional information lives in separate tables, absent as a missing row |
| Missing values in query results | `(option T)` from outer joins, empty aggregates, scalar subqueries | none, by restricting those operators |
| Logic | two-valued | two-valued |
| SQL | full SQL, NULL lowered to options | a null-free subset: no `LEFT JOIN`, and different results for keyless aggregates over empty input and for empty scalar subqueries |
| Schema design | unchanged | every optional attribute is its own table, with a foreign key and an entry in its entity's declaration |
| Engine work | expression semantics, optimizer, lowering | foreign keys, governance of attribute tables by their entity's policies, operator restrictions |
| Fidelity to the relational model | values only, but `none` is a special value | strict: every relation holds only values |

**It was rejected** because it gives up full SQL, and with it differential testing against Postgres. Every optional attribute would also become a table that must be governed by its entity's policies; otherwise it would be ungoverned and open to every principal.

The two compose in one direction: an option-typed IR can still hold decomposed schemas, and the book can recommend decomposition as schema design. The foreign keys that design needs are useful on their own, and are [ADR-0035](0035-enforced-foreign-keys.md).

## Future Work

- A `Datum::None` rename, and option-aware client result types.
- Distinguishing kinds of absence (Codd's "missing but applicable" and "inapplicable") with distinct option types, if an application needs it.

## References

- [ADR-0003](0003-order-preserving-tuple-encoding-and-row-format.md), [ADR-0010](0010-typed-relational-ir.md), [ADR-0014](0014-rule-based-optimizer.md), [ADR-0016](0016-security-expression-sublanguage.md), [ADR-0024](0024-expression-evaluation-with-postgres-semantics.md), [ADR-0032](0032-authorization-completeness.md), [ADR-0035](0035-enforced-foreign-keys.md)
- Backlog `array-element-nullability`
- E. F. Codd, *The Relational Model for Database Management: Version 2* (1990), chapter 8: two kinds of marks.
- C. J. Date and H. Darwen, *Databases, Types, and the Relational Model: The Third Manifesto* (3rd ed., 2006).
- C. J. Date, *SQL and Relational Theory* (3rd ed., 2015), chapter 4: "No duplicates, no nulls".
- Postgres documentation: unique indexes and `NULLS [NOT] DISTINCT`; `IS DISTINCT FROM`.
