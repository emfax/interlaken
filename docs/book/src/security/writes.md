# Writes and write checks

Writes are authorized like reads, with the `insert`, `update` and `delete` actions, in the style of Postgres row-level security. Two kinds of check apply:

- **Which existing rows a statement may touch.** An update or delete starts from rows the principal can read, and changes only those the `update` or `delete` policies also allow. Other rows are skipped silently, as with Postgres's `USING`. A delete with no applicable delete policy deletes nothing, and isn't an error.
- **Which rows a statement may write.** Every row an insert writes must be allowed by the `insert` policies, and every row an update writes, as it is after the update, by the `update` policies. A row that isn't allowed fails the whole statement with `42501`, "permission denied for table docs", as with Postgres's `WITH CHECK`. The message names the table, never the row's values.

```rls
(policies
  (permit (read) Doc true)
  (permit (insert update delete) Doc (== resource.owner principal)))
```

With these policies, a user may:
- read every document;
- insert documents only as their owner;
- update and delete only their own documents, and can't give a document away, because the updated row must still be theirs.

For user 2:

| Statement | Result |
| --- | --- |
| `(insert docs (values ... (row 200 "mine" 2)) ...)` with owner 2 | inserted |
| the same with owner 1 | `42501`, nothing inserted |
| `(update docs (get docs :as d) (set title "x"))` | changes only user 2's documents |
| `(update docs (filter (get docs :as d) (= d.id 101)) (set owner 1))`, on their own document 101 | `42501`: the new row fails the update policy |
| `(delete docs (get docs :as d))` | deletes only user 2's documents |

Each statement is atomic, so a failed check undoes every row the statement wrote.

## How checks are planned

- **Starting rows:** for update and delete, the starting rows are the input's read of the target table. They pass the `read` policies like any read, and then a filter for the `update` or `delete` residual.
- **New rows:** the rows an insert or update writes pass through a `WriteCheck` operator that evaluates the `insert` or `update` residual on each row before it is written, and fails the statement on the first row that doesn't pass.

Residuals that are always true add nothing to the plan. One that is always false makes an insert fail on its first row, and an update or delete change nothing.

Write checks see the row as written, with real values, never masked ones (see [Column policies and masks](masks.md#where-masks-apply)).

## Membership changes

Writing a membership edges table changes authorization itself: adding a user to a group grants whatever the group is granted. Edges tables are governed only if they back an entity type. Ungoverned, any principal can write them, so either declare them as an entity type with policies, or make sure only trusted code writes them. A membership change applies to later statements in the same transaction, and to other transactions once it commits (see [Entities and memberships](../ddl/entities.md)).

The example database's `user_groups` table backs no entity type, so in this book's examples any principal could add itself to a group. A real deployment should govern it.
