# Memory

Interlaken has no spilling to disk yet. An operator that would need more memory than it is allowed fails its query cleanly instead, with `53200`, rather than letting one query exhaust the process.

## Per-query budgets

Each statement has a memory budget, `EngineOptions::query_budget`, 256 MiB by default. Operators that hold data reserve memory against it as they read, so a query fails before buffering its whole input:

| Operator | Holds |
| --- | --- |
| hash join | the build side's rows and their keys |
| aggregation | groups, their states, and the arguments of `distinct` aggregates |
| sort and top-N | buffered rows (a top-N only its current best rows) |
| `distinct` | rows seen |
| set operations | rows seen and their counts |
| `nest` | keys, fields and groups |
| update and delete | the primary keys already changed |

Streaming operators hold only the few batches in flight and don't reserve: scans, filters, projections, limits, the probe side of a join, inserts. A streaming query over any number of rows runs in a small, constant amount of memory.

Exceeding the budget fails the statement with `53200`, naming the operator: `Sort exceeded the query memory budget`. The statement's writes are undone, and its memory is returned at once, as it is when a query finishes, fails or is cancelled. `EXPLAIN ANALYZE` shows each blocking operator's `peak_memory`.

Reservations match the memory operators really use, to within about 10%, as measured with a counting allocator. At the default budget, a sort holds about two million rows of eight columns, and a hash join builds from about 1.2 million.

To fit large blocking work in its budget:
- **Sort less:** a `limit` above a `sort` runs as a top-N, which holds only the rows it will return.
- **Filter early:** filter before joining and aggregating.
- **Build small:** put the smaller input on the build (right) side of a join.
- **Raise the budget:** if the memory is there.

## The node pool

A per-query budget bounds one query, but many concurrent queries can still exhaust a container. `EngineOptions::node_memory_limit` sets a cap shared by every query of the engine. A reservation that would cross it fails its query with `53200` ("... exceeded the node memory limit"), and other queries carry on. With no limit, the default, only the per-query budgets apply. `Engine::memory_pool()` returns the pool, whose `used()` and `peak()` show current and peak reservations.

Embedded use, where Interlaken shares a process with its application, needs no pool unless queries are large or many. In a container, set one. The recommended size is **half of the memory left after storage's own use**. The other half covers what no reservation counts:

- allocator overhead;
- the batches streaming operators have in flight;
- results collected with `Session::query`;
- transactions' buffered writes, bounded separately by `StorageOptions::max_write_set_bytes`.

Storage uses:

- a block cache of 32 MiB, not yet configurable;
- memtables of up to `StorageOptions::memtable_bytes` each for two keyspaces, 64 MiB by default, briefly more while full ones are flushed.

For a 4 GiB container with the defaults, about 3.8 GiB is left after storage, so the recommendation is a pool of about 1.9 GiB. That admits seven concurrent queries at their full default budget.

## Results

`Session::query` collects every row of a result in memory, outside any budget. For large results, use `query_stream`, which holds one batch at a time (see [Streaming results](../transactions/streaming.md)).
