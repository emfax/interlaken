---
id: 0036
title: Statistics and Cost-Based Planning
status: proposal  # proposal | accepted | implemented | superseded
tags: [optimizer, planner, storage, engine, security]
feature-flag:
created: 2026-11-22
accepted:
implemented:
spec-sections: ["ir-design.md#optimizer", "executor-design.md#physical-planning"]
superseded-by:
---

# ADR-0036: Statistics and Cost-Based Planning

## Status

Proposal.

## Summary

Collect statistics about table contents and use them to choose plans by estimated cost:
- **Statistics:** row counts, and per column the NULL fraction, distinct count, most common values and a histogram. `ANALYZE` collects them from a sample, explicitly or when enough rows have changed, and keeps them in an engine catalog table.
- **Advisory, never transactional.** They are read outside a statement's transaction, never conflict with writes, never change the schema version, and never affect a result, only how fast it arrives.
- **One estimator** in the IR crate computes row counts and selectivities from the statistics and the catalog. The rule-based optimizer uses it to order joins. The physical planner uses it to choose access paths, join algorithms and hash join build sides, with calibrated cost constants.
- **Foreign keys give exact join sizes** (ADR-0035).
- **Missing statistics change nothing.** Every choice falls back to today's heuristics, including ADR-0033's run-time choice between lookup and hash joins.

## Context

### How plans are chosen today

- **The optimizer is rule-based** (ADR-0014): simplification, decorrelation, pushdown under the security barrier, join elimination, pruning and order. *"Joins keep their written order: without statistics, reordering would be guesswork."*
- **The physical planner uses heuristics** (ADR-0028, `exec::access`):
  - An access path is scored by the shape of the predicate: a unique key pinned, then the longest bound prefix, then a range, then the primary key. A one-row equality on a non-unique index beats a full scan of a ten-row table, and a range that selects 90% of a table beats a full scan.
  - Hash joins always build the right input.
- **Lookup versus hash join is decided at run time** (ADR-0033). The operator reads up to twice the left rows of the right input before deciding, because *"the planner has no statistics to compare the inputs"*. ADR-0033 lists *"statistics would let the planner choose lookups up front instead of racing the inputs"* as a consequence to revisit.
- **Join order follows the query.** A query written `orders ⋈ customers ⋈ (filter regions (= name "EU"))` joins all orders to all customers before applying the selective filter on regions.
- **Foreign keys** (ADR-0035) give the one cardinality fact the planner can use without statistics, but nothing reads estimates, so ADR-0035 deferred its cardinality estimates to this ADR.

### What storage can and can't report

- **No per-table sizes.** Every table lives in one versioned `data` keyspace under its table-ID prefix (ADR-0002), so storage has no cheap size per table. Counting means scanning.
- **No transactional counters.** A row count kept in a row that every insert updates would make every pair of concurrent writers to a table conflict (ADR-0004).
- **The sequencer sees every change.** The commit sequencer (ADR-0005) sees every committed write set, so it can count row writes per table without any transactional state.

### Constraints

- **Security.** Statistics describe rows a principal may not see: most common values, histogram bounds, row counts of governed tables. Clients can't read engine tables, but `EXPLAIN` could expose estimates, so the privilege that already decides what `EXPLAIN` shows, `read-policies` (ADR-0031), should cover them.
- **Correctness.** A plan's result must never depend on statistics, and the differential tests compare plans against the reference interpreter regardless of how they were chosen.
- **Concurrency.** Collecting and reading statistics must not add conflicts to the optimistic transaction model.

## Decision

### Statistics

For each user table, `ANALYZE` records:

| Statistic | Of | Used for |
| --- | --- | --- |
| row count | the table | every estimate |
| average stored row width | the table | scan and memory costs |
| NULL fraction | each column | `is-null`, lifted comparisons, foreign key joins |
| distinct count | each column | equality selectivity, join sizes, group counts |
| most common values with frequencies (up to 32) | each comparable scalar column | equality on skewed data |
| equi-depth histogram (up to 100 buckets, excluding the common values) | each comparable scalar column | range selectivity |
| distinct count of each leading prefix | each index | multi-column equality on indexes |

Arrays, records and `jsonb` get only the NULL fraction and width.

### Collection

- **`ANALYZE`** reads a uniform sample of up to 30,000 rows per table, by reservoir sampling over a full scan, at one snapshot in a read-only transaction. Read-only transactions never conflict (ADR-0004).
  - Row counts are exact, from the scan.
  - Distinct counts come from a HyperLogLog sketch over the whole scan rather than the sample, since sample-based distinct estimators are unreliable on skewed data.
  - Frequencies and histograms come from the sample.
- **Writing statistics** happens in a separate short transaction that writes only the statistics table. That table is advisory: no statement reads it through its own transaction, so writing it never conflicts with anything.
- **Session API:** `Session::analyze(table)`, and `Session::analyze_all()`, both requiring `admin`.
- **Automatic analysis.** The commit sequencer adds each committed transaction's row-key writes per table into an in-memory change counter, without transactions, persisted with the last-commit timestamp in `meta`. When a table's changes since its last analysis exceed 50 rows plus 10% of its row count, a background task analyzes it at low priority on the storage threads, like garbage collection (ADR-0008). `EngineOptions::auto_analyze` turns this off; it defaults to on.
- **Costs to bound:** a full scan per analysis. Large tables and frequent changes are the case to measure. Block sampling instead of a full scan is future work.

### Storage and reading

- **The table.** The engine catalog table `engine_statistics` (table ID, column position or −1 for the table, kind, payload `bytea`), with a per-table `analyzed_at` timestamp and the change count at analysis.
- **Reading.** An engine-wide `StatisticsCache` holds the decoded statistics. It is refreshed after each analysis and on open, by reading the table in its own read-only transaction, never through a statement's.
- **Plans don't pin statistics.** Prepared plans keep their resolved form and are planned again on each run (ADR-0031), so they pick up new statistics. Statistics don't change the schema version: they don't invalidate plans, and DDL doesn't wait for them.
- **DDL.** Dropping a table or column drops its statistics. Adding a column, index or foreign key doesn't analyze; the next analysis covers it.

### The estimator

The estimator lives in the IR crate, beside the optimizer that uses it: `ir::estimate`, with a `Statistics` trait that the engine implements over its cache and tests implement in memory. It estimates the output rows of every logical operator, and its estimates feed the physical planner's costs.

**Selectivity of predicates**, on the normalized two-valued form (ADR-0034):

| Predicate | Selectivity |
| --- | --- |
| `(= c v)` on a common value | its frequency |
| `(= c v)`, otherwise | `(1 − null fraction − common frequencies) / (distinct − common count)` |
| `(< c v)`, ranges | histogram interpolation, plus common values in range |
| `(coalesce (lift (cmp a b)) false)` | as `cmp`, times the fraction where both are present |
| `is-null c` / `is-not-null c` | NULL fraction / its complement |
| `(= c d)` between columns | `1 / max(distinct)` |
| `and`, `or`, `not` | product, inclusion–exclusion, complement (independence assumed) |
| parameters | the column's average selectivity for the operator |
| anything else, and missing statistics | Postgres's defaults: 0.005 for equality, 1/3 for ranges, 0.5 otherwise |

**Operators:**
- **Joins:**
  - an equi-join is `|L|·|R| / max(distinct(L key), distinct(R key))`;
  - a foreign key join from child to parent is `|child| × (1 − NULL fraction of the referencing columns)`, exactly, whatever the statistics say about the parent;
  - semi and anti joins use the matching fraction.
- **Aggregates:** the product of the keys' distinct counts, capped at the input rows.
- **`Distinct`, `Nest` and `Limit`** follow the same rules.
- **`PolicyFilter`:** its residual is estimated like any predicate, over the resource row it is written on. Residual subqueries and calls the estimator can't see into get 0.5.

### The cost model

- **Units.** Costs are in microseconds on the reference machine. The constants are measured, not guessed, by a benchmark like ADR-0033's (`benches/cost_constants.rs`): per-row costs of a sequential scan, an index range scan, a point lookup, building and probing a hash table, sorting, and a storage call's fixed overhead.
- **Configuration.** `PlanOptions::costs` carries the constants, defaulting to the measured ones, for deployments to recalibrate.
- **Memory.** A hash join or sort whose estimated input exceeds the query's memory budget (ADR-0027) costs as unbounded and loses to any plan that streams, since spilling is future work.

### Plan choices that use costs

1. **Access paths.** The planner chooses the cheapest of a full scan, the primary key and each usable index, by estimated rows and cost. This replaces the score. Without statistics it keeps the score.
2. **Lookup or hash join.** With statistics for both inputs, the planner chooses at plan time, `IndexLookup` against a hash join, from ADR-0033's measured per-row costs. Without them, or when the left input's estimate is unreliable (below a parameterized predicate, or under a `PolicyFilter` with an opaque residual), it keeps ADR-0033's run-time race.
3. **Hash join build side.** The smaller estimated input builds. For outer joins, swapping sides swaps the join kind; semi and anti joins don't swap.
4. **Join order.** A new optimizer phase reorders each *region* of inner joins and filters between outer, semi and anti joins, applies, aggregates and set operations. It uses dynamic programming over connected subgraphs (DPccp) for regions of up to 10 relations, and greedy operator ordering beyond. Without statistics for a region's tables, it keeps the written order.

   Security barriers are untouched: a `PolicyFilter` and its masks stay wrapped around their `Get` as a leaf of the region, and join conditions are placed only above both of their inputs' barriers. Reordering may evaluate a condition on rows the written order would have filtered out first. Like pushdown, it then may raise an error the written plan wouldn't have, which the optimizer's contract allows (ADR-0014).

### Security

- **Statistics are engine tables.** Clients can't read them, as with every `engine_*` table.
- **`EXPLAIN` hides estimates** from principals without `read-policies`, as it already hides policy logic and masks (ADR-0031, ADR-0032). An estimate over a governed table counts rows the principal may not see. `EXPLAIN ANALYZE`'s actual row counts stay visible, since they count rows that passed the barrier.
- **Plan choice can still reveal something through timing**, as it does in Postgres: a query may run faster because hidden rows are few. No ADR addresses timing channels yet: ADR-0021's leak suite tests only timing-independent probes. This ADR records the new one rather than closing it (see Open Questions).

### Observability

`EXPLAIN` shows `est=N` on each operator for principals with `read-policies`. `EXPLAIN ANALYZE` shows estimated and actual rows side by side. A metric (ADR-0033's metrics) records the q-error, `max(est/actual, actual/est)`, of each top-level estimate, so poor estimates show up in monitoring before they show up as slow queries.

## Implementation Phases

- [ ] **Phase 1: Statistics.**
  - `ANALYZE` with reservoir sampling and HyperLogLog;
  - `engine_statistics`, `StatisticsCache` and `Session::analyze`;
  - the sequencer's change counters persisted in `meta`, and automatic analysis;
  - DDL cleanup;
  - the book's operations chapter.
  - *Testing:* statistics of generated tables against exact values (distinct counts within HyperLogLog's error, histograms with equal-depth buckets), no conflicts with concurrent writers at every isolation level, and counters surviving restarts.
- [ ] **Phase 2: Estimator.**
  - `ir::estimate` and the `Statistics` trait;
  - selectivities and operator estimates, foreign key joins included;
  - `est=` in `EXPLAIN`, privileged;
  - the q-error metric.
  - *Testing:* estimates against true row counts on generated data with uniform, skewed and correlated columns, with q-error bounds per predicate kind, and exact foreign key join estimates.
- [ ] **Phase 3: Physical choices.**
  - cost constants and their benchmark;
  - cost-based access paths;
  - plan-time lookup or hash join with the run-time race as fallback;
  - build-side selection.
  - *Testing:* plan tests for the cases the heuristics get wrong (small tables, unselective ranges, skewed equality), and the differential tests run with statistics on and off.
- [ ] **Phase 4: Join order.**
  - region detection, DPccp and greedy ordering, barrier-preserving placement of conditions.
  - *Testing:* the IR optimizer differential test with statistics, random data and random join graphs, comparing results with the interpreter; plan tests on a small star schema; a benchmark of a selective three-way join written in the worst order.
- [ ] **Phase 5: Calibration.**
  - measured constants on the target deployment hardware (with backlog `fsync-stalls-other-hardware`);
  - documentation of recalibration;
  - review of q-error metrics from the benchmarks.

## Consequences

### Positive

- Queries no longer depend on being written in a good order: selective tables join first, whatever the order of the text.
- Access paths stop preferring an unselective index to a cheap scan, and small tables stop costing a lookup race.
- Foreign keys finally feed estimates, exactly.
- Statistics never cause conflicts, never block DDL and never change results, so they can only make plans faster or slower, not wrong.
- Estimates are visible to those allowed to see them, and their quality is measured.

### Negative

- **Plans change when statistics change.** A query that was fast can become slow after an analysis; plan stability (pinned plans, hints) is future work.
- **Each analysis scans the whole table.** On large tables that costs I/O and storage-thread time, which automatic analysis makes recurring.
- **Assumptions of independence and uniformity** misestimate correlated columns and skewed joins. Multi-column statistics are future work.
- **The planner gets more complex.** Join ordering adds a search with its own planning time, which is bounded by the region size limit.
- **A new timing channel**, recorded rather than closed.

## Open Questions

- Should automatic analysis also run on open for tables never analyzed, or wait for the change threshold?
- Is 30,000 rows the right sample size? Postgres uses 300 × the statistics target (30,000 at its default).
- Should the change counters be exact per table, by counting distinct row keys, or count every row write, as proposed, which overcounts rows updated twice in one transaction?
- Should timing channels enter the security model (ADR-0015, ADR-0021)? Statistics add one: a plan chosen from hidden rows' statistics runs faster or slower.
- Should a session be able to turn statistics off (`PlanOptions::statistics = false`) for debugging plans, and should `EXPLAIN` say which choices fell back to heuristics?

## Future Work

- Block sampling instead of full scans for large tables.
- Multi-column statistics (functional dependencies, multi-column distinct counts) for correlated predicates.
- Plan stability: pinned plans, hints, or plan baselines.
- A Cascades-style memo (ADR-0014's future work) once the rule set and cost model justify it.
- Spilling joins and sorts (ADR-0027's future work), which changes how memory enters the cost model.
- Statistics on expressions and on nested data for GraphQL (ADR-0012).

## References

- [ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md), [ADR-0004](0004-optimistic-transactions-and-isolation-levels.md), [ADR-0005](0005-commit-sequencer-group-commit-and-durability.md), [ADR-0008](0008-version-garbage-collection.md), [ADR-0012](0012-nesting-and-decorrelation-for-graphql.md), [ADR-0014](0014-rule-based-optimizer.md), [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0021](0021-ir-correctness-and-security-testing.md), [ADR-0027](0027-query-memory-management.md), [ADR-0028](0028-physical-operators-planning-and-explain.md), [ADR-0031](0031-engine-sessions-and-storage-backed-catalog.md), [ADR-0032](0032-authorization-completeness.md), [ADR-0033](0033-executor-performance.md), [ADR-0034](0034-two-valued-logic-with-an-option-type.md), [ADR-0035](0035-enforced-foreign-keys.md)
- Backlog `fsync-stalls-other-hardware`.
- G. Moerkotte and T. Neumann, "Analysis of Two Existing and One New Dynamic Programming Algorithm for the Generation of Optimal Bushy Join Trees without Cross Products", VLDB 2006 (DPccp).
- P. Flajolet et al., "HyperLogLog: the analysis of a near-optimal cardinality estimation algorithm", 2007.
- V. Leis et al., "How Good Are Query Optimizers, Really?", VLDB 2015 (q-error and the effect of misestimates).
- Postgres documentation: `ANALYZE`, `pg_statistic`, planner statistics and default selectivities.
