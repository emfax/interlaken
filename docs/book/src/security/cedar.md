# Writing policies in Cedar

[Cedar](https://www.cedarpolicy.com) is a policy language designed for authorization, with a formal semantics. Interlaken's Cedar policies are ordinary Cedar, validated against a schema generated from the catalog. Cedar is the language to choose when policy changes should be provable (see [Changing policies](proposals.md)).

```cedar
permit (principal, action == Action::"read", resource is Doc)
when { resource has owner && resource.owner == principal };

permit (principal, action == Action::"read", resource is Doc)
when { principal in Group::"7" };

forbid (principal, action == Action::"read", resource is Doc)
when { resource has title && resource.title like "draft:*" };
```

Owners read their documents and members of group 7 read every document, but nobody reads a document whose title starts with `draft:`.

## The generated schema

Policies are validated in Cedar's strict mode against a schema the engine generates from the catalog's entity declarations:

- **Entity types:** each declared entity type becomes a Cedar entity type, and each membership adds its group type to the member type's parents. In the example database, `User in [Group]`.
- **Attributes:** each attribute becomes a Cedar attribute.
  - Its type follows its column: `bool` is `Bool`, the integers are `Long`, `text` is `String`, and arrays of those are `Set`s.
  - A reference attribute has the referenced entity type, so `resource.owner` is a `User`.
  - Columns of other types (`float8`, `date`, `timestamptz`, `uuid`, `bytea`, `jsonb`) aren't visible to Cedar.
- **Optional attributes:** a nullable column is an optional attribute, which a policy must test with `has` before reading. Strict validation rejects `resource.owner == principal` on a nullable `owner`; write `resource has owner && resource.owner == principal`.
- **Actions:** `Action::"read"`, `Action::"insert"`, `Action::"update"` and `Action::"delete"` apply to every entity type, as principal and as resource.
- **Context:** `context` has `time` (`Long`), `ip`, `auth_method` and `session_id` (`String`), and `app`, whose record type comes from the [policy settings](proposals.md#policy-settings).

A policy that doesn't validate is rejected when it is proposed. This one fails validation, so the proposal can't be activated (`55000`):

```cedar,error=55000
permit (principal, action == Action::"read", resource is Doc)
when { resource.owner == principal };
```

## Writing policies

A policy's scope names the principal, action and resource it applies to, and `when` and `unless` clauses add conditions:

```cedar
permit (
  principal is User,
  action in [Action::"read", Action::"update"],
  resource is Doc
)
when { resource has owner && resource.owner == principal };

permit (principal is User, action == Action::"read", resource is Doc)
when {
  resource has owner && resource.owner has dept &&
  principal has dept && resource.owner.dept == principal.dept
};

permit (principal is User, action == Action::"read", resource is Folder)
unless { resource has owner && resource.owner == User::"3" };
```

These permit:
- owners to read and update their documents;
- users to read documents owned by anyone in their department, following the `owner` reference to the owning user's `dept`;
- users to read every folder not owned by user 3.

Every policy should name its resource type with `resource is T`. A policy applies only to the resource types it can match, so name exactly the tables it governs.

Interlaken evaluates Cedar with type-aware partial evaluation. Conditions on the principal and context are decided before the query runs, and the rest becomes a filter on the table. A residual containing something the engine can't translate fails closed: the statement is denied rather than run without the condition. Untranslatable constructs are extension types (`ip`, `decimal`, `datetime`), entity tags and unknown values. Prefer comparisons, `has`, `in`, `like`, `contains` and the set operators.

## Column policies

A Cedar policy annotated `@column("title")` governs reading the `title` attribute rather than whole rows. `@mask` gives the value shown where it denies:

```cedar
permit (principal, action == Action::"read", resource is Doc)
when { resource has owner && resource.owner == principal };

permit (principal, action == Action::"read", resource is Doc)
when { principal in Group::"7" };

@column("title") @mask("mask_last4")
permit (principal, action == Action::"read", resource is Doc)
when { resource has owner && resource.owner == principal };
```

Group 7 reads every document, but sees titles in full only on documents they own, and with all but the last four characters masked on the rest. See [Column policies and masks](masks.md).

## Administrators and other undeclared principals

Cedar policies can only name declared entity types, so a principal like `Admin::"root"` can't be granted anything in Cedar. Use a principal of a declared type for administration, or RLS policies (see [Governed tables](model.md#governed-tables)).

## Proving changes

Because Cedar has a formal semantics, a prover node with the SMT solver can prove properties of a policy change over every possible request and entity, not just today's data:
- that no policy can raise an error;
- that the change grants nothing the current set denies;
- that it stays within declared invariants.

See [Changing policies](proposals.md).
