# Entities and memberships

Policies are written about *entities*: users, groups, documents. An entity type is backed by a table: each row is an entity, its key column identifies it, and some of its columns are attributes that policies can read. A *membership* declares that entities of one type can belong to entities of another, such as users in groups, and records those memberships in tables the engine keeps consistent.

Declaring an entity type puts its table under policy control. Read [The security model](../security/model.md) before declaring entity types on a database that already has data and users.

## Declaring entity types

```rust
# use interlaken_engine::catalog::{AttributeSpec, ColumnSpec, EntitySpec, MembershipSpec, TableSpec};
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# let table = |name: &str, columns: &[(&str, &str)], key: &[&str]| TableSpec {
#     name: name.into(),
#     columns: columns.iter().map(|(n, t)| ColumnSpec::new(*n, parse_type(t).unwrap())).collect(),
#     primary_key: key.iter().map(|k| (*k).into()).collect(),
# };
# session.create_table(&table("users", &[("id", "int8"), ("name", "text"), ("dept", "(option text)")], &["id"])).await?;
# session.create_table(&table("groups", &[("id", "int8"), ("name", "text")], &["id"])).await?;
# session.create_table(&table("docs", &[("id", "int8"), ("title", "(option text)"), ("owner", "(option int8)")], &["id"])).await?;
# session.create_table(&table("user_groups", &[("member", "int8"), ("grp", "int8")], &["member", "grp"])).await?;
# session.create_table(&table("user_group_closure", &[("descendant", "int8"), ("ancestor", "int8"), ("paths", "int8")], &["descendant", "ancestor"])).await?;
session
    .declare_entity(&EntitySpec {
        name: "User".into(),
        table: "users".into(),
        key: "id".into(),
        attributes: vec![AttributeSpec::new("name"), AttributeSpec::new("dept")],
    })
    .await?;
session
    .declare_entity(&EntitySpec {
        name: "Group".into(),
        table: "groups".into(),
        key: "id".into(),
        attributes: vec![AttributeSpec::new("name")],
    })
    .await?;
session
    .declare_entity(&EntitySpec {
        name: "Doc".into(),
        table: "docs".into(),
        key: "id".into(),
        attributes: vec![
            AttributeSpec::new("title"),
            AttributeSpec::reference("owner", "owner", "User"),
        ],
    })
    .await?;
session
    .declare_membership(&MembershipSpec {
        member: "User".into(),
        group: "Group".into(),
        edges: "user_groups".into(),
        edge_columns: ("member".into(), "grp".into()),
        closure: "user_group_closure".into(),
        closure_columns: ("descendant".into(), "ancestor".into(), "paths".into()),
    })
    .await?;
# Ok(())
# }
```

An `EntitySpec` names:

- **`name`:** the entity type, as policies write it (`User`, `Doc`). It is unique, and each table backs at most one entity type.
- **`table` and `key`:** the backing table, and the column whose value identifies each entity. `EntityUid::long("User", 2)` is the user whose `id` is 2, and `EntityUid::str` is for text keys.
- **`attributes`:** the columns policies can read, each under an attribute name.
  - `AttributeSpec::new("dept")` exposes the column of the same name.
  - `AttributeSpec::reference("owner", "owner", "User")` declares that the column holds the key of a `User`, so policies can follow it: `resource.owner.dept` is the department of the document's owner.
    A reference isn't checked: a key with no `User` row makes that entity missing, and policies that read its attributes fail. A [foreign key](foreign-keys.md) on the same column makes the key exist, and must refer to the same entity type's key.
  - A NULL column means the attribute is absent, which policies must allow for (see [Writing policies in Cedar](../security/cedar.md)).

Attributes that policies don't need can be left out; policies can't read undeclared columns. Entity declarations change only the catalog: the table and its rows stay as they are. `drop_entity(name)` removes a declaration, unless a membership uses it.

## Memberships

A membership needs two tables, which you create:

- **The edges table** holds direct memberships, one row per (member, group) pair, such as `user_groups(member, grp)`. You write it like any table.
- **The closure table** holds every (descendant, ancestor) pair connected by one or more edges, with the number of distinct paths between them, such as `user_group_closure(descendant, ancestor, paths)`. The engine writes it.

`MembershipSpec` names both tables and their columns, in the order shown. Give the member and descendant columns the type of the member entity's key, the group and ancestor columns the type of the group entity's key, and `paths` the type `int8`. Give the closure table a primary key of (descendant, ancestor), as above. A declaration whose columns don't exist or have other types fails with `42P16`, naming the column and the type it needs. Nullability doesn't matter.

Policies test membership with `in`, as in `(in principal (entity Group 7))` or Cedar's `principal in Group::"7"`. That test becomes one indexed lookup in the closure table, however deep the hierarchy.

### Closure maintenance

Every insert, update or delete on an edges table updates the closure table in the same statement:
- Adding an edge from `m` to `g` adds paths from everything that reaches `m` to everything `g` reaches.
- Removing an edge subtracts them.
- A pair whose path count reaches zero is deleted.

Counting paths, rather than recording reachability, keeps the closure right when an edge is removed while another path still connects the pair.

```ir
(plan (insert user_groups (values (columns member grp) (row 2 7)) (columns member grp)))
```

After this, `user_group_closure` holds `(2, 7, 1)`, and a policy granting group 7 access applies to user 2 in the same transaction. Writing the closure table yourself would make it disagree with the edges, so don't. Policies decide whether a principal may write an edges table, as for any governed table. Memberships are part of the transaction:
- **Cycles:** an edge that would make the hierarchy cyclic fails the statement with `23514`.
- **Concurrent changes:** two transactions changing memberships so that the counts would disagree conflict, and one fails with `40001`.
- **Authorization conflicts:** under SERIALIZABLE, a writing transaction authorized by a membership conflicts with a concurrent transaction that removes it (see [Authorization is transactional](../security/model.md#authorization-is-transactional)).

### Hierarchies across types

A membership can relate a type to itself, such as folders in folders. Hierarchies can also span types, such as documents in folders in folders. Each pair is written to the closure table of its two types, so every composition of declared memberships must itself be declared. With `Doc in Folder` and `Folder in Folder`, the transitive `Doc in Folder` pairs go in the `Doc in Folder` closure, which therefore holds a document's every enclosing folder, not just its direct one.

`drop_membership(member, group)` removes a membership's declaration. Its tables and rows stay.

## The example database

This book's examples declare `User`, `Group`, `Folder` and `Doc` over the tables of the [example database](../ir/syntax.md#the-example-database), with users in groups as the one membership.
