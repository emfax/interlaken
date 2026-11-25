# Namespaces

A namespace groups tables, indexes, entity types, memberships, foreign keys and principal types under one name, so that several applications, or parts of one, can share a database without their names colliding. Each namespace has an owner, who can change its schema and policies without the `admin` privilege.

Every database has the namespace `public`, owned by its first administrator. Everything created without a namespace goes there, so a database that never uses namespaces works exactly as before.

## Creating and dropping namespaces

```rust,ignore
let app_owner = EntityUid::long("User", 1);
root.create_namespace("app", &app_owner).await?;
root.set_namespace_owner("app", &EntityUid::long("User", 2)).await?;
root.drop_namespace("app", false).await?; // only if it's empty
root.drop_namespace("app", true).await?;  // drops everything in it
```

In IR text:

```ir
(create-namespace app :owner (entity User 1))
(set-namespace-owner app (entity User 2))
(create-table app.todos (column id int8) (column owner int8) (primary-key id))
(drop-namespace app :cascade true)
```

- **Names** are identifiers: letters, digits and `_`, not starting with a digit, `engine_` or `sys_`.
- **`create_namespace` and `drop_namespace`** need `admin`.
- **`set_namespace_owner`** needs `admin` or the current owner.
- **Cascade** drops, in one transaction, the namespace's foreign keys, memberships, entity types, principal types, tables with their indexes, and its policy set. It also drops foreign keys in other namespaces that refer to its tables. `public` can't be dropped.

## Names and the search path

An object in a namespace is named `namespace.name`, such as `app.todos`, everywhere a name is written: in plans (`(get app.todos :as t)`), in DDL specs, and in policies. Column references stay qualified by the table's alias, or by the last part of its name: `todos.title`.

An unqualified name is looked up through the session's *search path*, the namespaces to try in order:

```rust,ignore
session.set_search_path(["app", "public"]);
session.query("(plan (get todos))", Vec::new()).await?; // app.todos if it exists, else public.todos
```

- The default search path is `["public"]`.
- Namespaces in the path that don't exist are skipped.
- A new object named without a namespace goes in the first namespace of the path that exists.
- `public.todos` always means the table in `public`.
- Prepared plans keep the tables they resolved, whatever the search path is later.

## Owners

A namespace's owner may, without `admin`:
- create, change and drop its tables, indexes, entity types, memberships, foreign keys and principal types;
- propose, verify and activate its policy set (see [Changing policies](../security/proposals.md#policy-sets-per-namespace));
- give the namespace to another principal.

DDL that spans namespaces needs every namespace involved: a foreign key from `app.orders` to `crm.customers` needs the owners of both, or `admin`. Owning a namespace grants no access to its data: queries are governed by policies, as for everyone.

## Entity types in namespaces

An entity type declared over a table in a namespace is in that namespace too: `Todo` over `app.todos` is `app.Todo`. Policies name it that way: `app::Todo` in Cedar, and `app.Todo` in the RLS sublanguage. Entity types in `public` keep their plain names.
