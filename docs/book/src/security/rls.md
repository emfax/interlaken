# Writing policies in the RLS sublanguage

The RLS sublanguage is Interlaken's own policy language, in the style of Postgres row-level security, written as S-expressions like the IR. It has the same semantics as Cedar: deny by default, forbid overrides permit, no NULLs, and a condition that raises an error doesn't hold. It is simpler to write. It can also name principal types that aren't declared entity types. But its policies can't be proved by the SMT prover (see [Changing policies](proposals.md)).

```rls
(policies
  (permit (read insert update delete) Doc (is principal Admin))
  (permit (read update) Doc (== resource.owner principal))
  (permit (read) Doc (in principal (entity Group 7)))
  (forbid (read) Doc (like resource.title "draft:*")))
```

These say:
- administrators may do anything with documents;
- owners may read and update theirs;
- members of group 7 may read every document;
- nobody may read a document whose title starts with `draft:`.

## Policies

```text
(policies POLICY...)
POLICY: (permit (ACTION...) TYPE CONDITION [:column NAME [:mask MASK]])
      | (forbid (ACTION...) TYPE CONDITION [:column NAME [:mask MASK]])
ACTION: read | insert | update | delete
```

Each policy names its actions, the entity type of the resources it governs, and a condition. `:column` and `:mask` make it a column policy (see [Column policies and masks](masks.md)). A document is one `(policies ...)` list, the whole active policy set. Every change replaces it entirely.

## Conditions

| Form | Meaning |
| --- | --- |
| `true`, `false`, `42`, `"text"` | literals: booleans, 64-bit integers, strings |
| `principal`, `resource`, `context` | the principal, the row being decided, and the request context |
| `resource.owner.dept` | an attribute path, following references |
| `(entity TYPE KEY)` | an entity, such as `(entity Group 7)` or `(entity User "ada")` |
| `(set E...)`, `(record (NAME E)...)` | a set or record value |
| `(attr E NAME)` | attribute `NAME` of `E` |
| `(has E NAME)`, `(has PATH)` | whether an attribute is present |
| `(is E TYPE)` | whether `E` is an entity of type `TYPE` |
| `(in E1 E2)` | whether entity `E1` is `E2` or belongs to it, transitively; or, with a set on the right, belongs to some member of it |
| `(contains SET E)`, `(contains-all SET1 SET2)`, `(contains-any SET1 SET2)` | set membership |
| `(like E "glob*")` | string match; `*` matches any run of characters |
| `(== a b)`, `(!= a b)`, `(< a b)`, `(<= a b)`, `(> a b)`, `(>= a b)` | comparisons |
| `(+ a b)`, `(- a b)`, `(* a b)` | integer arithmetic; overflow is an error |
| `(and E...)`, `(or E...)`, `(not E)`, `(if C THEN ELSE)` | logic, evaluated left to right, stopping as soon as the result is known |
| `(try E DEFAULT)`, `error` | catch an error, or raise one |

Values have no NULL:
- **Absent attributes:** a NULL column is an absent attribute, and reading an absent attribute is an error, so the condition doesn't hold. A permit over a NULL `owner` grants nothing, without needing `has`.
- **Forbids:** a forbid over an absent attribute forbids nothing. Guard forbids with `has`, or write them so they can't fail. The lint gate warns when one can.
- **Equality across types:** comparing values of different kinds with `==` is false, not an error.

## Examples

Owners, and their department, read; only owners write:

```rls
(policies
  (permit (read) User true)
  (permit (read) Doc (or (== resource.owner principal) (== resource.owner.dept principal.dept)))
  (permit (insert update delete) Doc (== resource.owner principal)))
```

`resource.owner.dept` follows the document's `owner` reference to the owning user's row. The engine plans that as a join, so it costs no more than writing the join yourself.

Tenants, from the application context:

```rls
(policies
  (permit (read) Folder (== context.app.tenant "acme"))
  (forbid (read) Folder (!= context.auth_method "mfa")))
```

Folders are readable only in the `acme` tenant, and only for sessions that authenticated with MFA. Both conditions are decided before the query runs, so the table is either read in full or not at all.

Tags, from an array attribute:

```rls
(policies
  (permit (read) Doc (contains resource.tags "public"))
  (permit (read) Doc (has resource.title)))
```

## Choosing between RLS and Cedar

| | RLS | Cedar |
| --- | --- | --- |
| Syntax | S-expressions, like the IR | Cedar |
| Validation | parse and type check | Cedar strict validation against the generated schema |
| Absent attributes | reading one makes the condition not hold | must be guarded with `has` to validate |
| Undeclared principal types | `(is principal Admin)` | can't be named |
| Proofs | no | never-errors, no broadening and invariants, on a prover node |
| Required by `proof-required` mode | no, can't be used | yes |

Both compile to the same residuals and plans. Each active policy set is in one language, and a proposal can switch languages.
