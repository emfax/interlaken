# EXPLAIN and policy visibility

`Session::explain(text, analyze)` returns the physical plan of a statement, one operator per line, indented under its parent:

```rust,ignore
let plan = session.explain("(plan (project (filter (get docs :as d) (= d.folder 10)) d.id d.title))", false).await?;
println!("{plan}");
```

The plan shown is the one that would run for the session's principal: after security lowering and optimization, with that principal's policies in it. Columns appear as `#binding.column`.

With `analyze` set to `true`, the statement runs and each operator shows what it did:
- `rows` and `batches` produced;
- `loops`, the number of times it ran, which is more than one inside an `apply`;
- `time`;
- for scans, `storage_calls` and `bytes_read`;
- for operators that buffer, `peak_memory` in bytes.

As in Postgres, `EXPLAIN ANALYZE` of a DML statement performs the writes, so wrap it in a transaction and roll back if you only want the plan.

## What principals without `read-policies` see

The plan contains the principal's policies, and policy logic can itself be sensitive. A condition like `owner.dept = 'legal'` reveals how access is decided. So for principals without the `read-policies` privilege, `EXPLAIN` replaces every part of the plan that security lowering added:

| In the plan | Shown as |
| --- | --- |
| a policy condition pushed into a scan | `TableScan docs policy: <policy>` |
| a policy filter (the barrier) | `PolicyFilter(docs)` with no condition |
| a write check | `WriteCheck(docs)` with no condition |
| a subplan added for a policy, such as a join to the owner's row or a closure lookup | `<policy> subplan` |
| a join condition linking such a subplan | `on <policy>` |
| a column mask | `<policy> mask` |

For example, user 1, under policies that let them read documents of their own department and mask others' titles:

```text
Project #4.0, #4.2
  Filter (= #4.1 10)
    Project [#1.0, #1.1, <policy> mask]
      PolicyFilter(docs)
        HashJoin left on <policy>
          TableScan docs
          <policy> subplan
```

The principal sees where policies apply and how much work they cost, but not what they say. With `read-policies` or `admin`, the same plan shows every condition, mask and subplan in full.

## How the redaction is exact

Security lowering marks everything it adds:
- **Subplans and values lists:** each gets an alias or column name with the reserved prefix `sec$`.
- **Scans and values:** each carries a flag saying a policy added it.

`EXPLAIN` hides exactly the marked parts, wherever the optimizer has moved them. A client can't forge the marks: the verifier rejects plans from clients that use the `sec$` prefix (see [What clients can't write](../ir/syntax.md#what-clients-cant-write)). A client therefore can't hide part of its own plan, nor make `EXPLAIN` reveal a policy by disguising it as client logic.
