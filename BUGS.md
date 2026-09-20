# Known bugs

This file records only defects that cannot be fixed in the current change
because an external blocker or genuine intractability prevents a fix. Fixable
defects belong in the implementation that discovers them; planned engineering
work and architecture limits belong in [PLAN.md](PLAN.md).

There are currently no defects that meet this file's inclusion criteria.
The geometric, ranked nearest-neighbor, inverted posting, signature, and
interval index-navigation,
checkpoint-maintenance capacity and publication handoff, concurrent-harness
port, integration-scratch, accepted-definition list, and statement-width
audits have no external blocker; their fixes are qualified in the
implementation and regression suites. The catalog-version and stored-query
dependency audits likewise found no external blocker: table definitions,
statistics, and dependency images use startup-sized pools, the shared
per-object dependency ceiling is configurable, and savepoint, exact-memory,
configured-exhaustion, checkpoint-retry, and cold-recovery regressions cover
them.
The dependent-object planning audit found no external blocker: every stored
query and ownership selection follows its independent configured catalog,
including slots above 127, and allocation-forbidden rollback plus checkpoint
retry and object-cold recovery qualify the complete cascade.
The row-version and spill-generation audit also found no external blocker:
both former eight-entry ceilings are startup-configurable, fully accounted,
and covered across exhaustion, reuse, checkpoint publication, and empty-cache
recovery.
The extended-statistics and BRIN-maintenance capacity audit found no external
blocker: the former table-derived ceiling and per-index inline array are
replaced by configured startup pools and qualified with maximum trigger
catalogs through checkpoint retry and object-cold recovery.
The procedural program-width audit found no external blocker: simple-query and
stored SQL programs, PL/pgSQL locals, branches, handlers, conditions, and loop
control now use statement-arena storage, with differential and object-cold
qualification beyond the former compiled limits.
The execution-effect audit found no external blocker: event-trigger object
graphs and volatile sequence replay now grow within statement memory, and
configured transaction DDL capacity is no longer capped at 256. Wide cascades
and 1,100-call statements are qualified allocation-free, differentially, and
after empty-cache object recovery. Retry-persistent effects use the statement
arena tail so CTE, CTAS, and materialized-view row-scratch rewinds cannot
invalidate recorded values.
The remaining execution-width audit also found no external blocker. Mutable
routine arguments and replay results now survive retries in statement memory,
cursor indexes are sized from `cursor_bytes`, and logical-message decoding
uses work memory. Regressions cross 1,024 calls and messages and 65,536 cursor
rows under the allocation guard, PostgreSQL differential execution, and
empty-cache recovery.
The residual inline-width audit found no external blocker. Effective search
paths are sized from their accepted byte boundary instead of a sixteen-entry
array, split table functions use exact statement-arena result slices, and
checkpoint deletion markers use the already bounded row overlay instead of an
unused per-table 1,024-entry roster. PostgreSQL 18 differential,
allocation-forbidden execution, delta-generation inspection, checkpoint
publication, and empty-cache recovery cover the former limits.
The SQL array-width audit found no external blocker. Array values now use the
durable 65,535-element boundary, exact statement-memory scratch, and
single-pass payload traversal. Text and binary input, aggregation, mutation,
variadic expansion, comparison, set expansion, output, checkpoint publication,
and empty-cache recovery are qualified beyond the former 1,024-element limit.
The same audit removed `format()`'s unrelated 4 KiB result ceiling.
The statement-list-width audit found no external blocker. Parser staging,
query rewrites, and execution scratch for statement lists use the statement
arena; per-row correlated-subquery merge scratch is pre-sized from true node
counts. The audit fixed a window-scope restore that truncation alone could not
preserve, made statement-arena exhaustion during parse a program-limit error
rather than a syntax error, and removed a silent cap that dropped correlated
subqueries in grouped output beyond 64. PostgreSQL 18 differential,
allocation-forbidden, and empty-cache recovery regressions cross the former
64-item and 256-row boundaries.
The JSON value-width audit found no external blocker. JSON containers, path
steps and results, and rendered text now use statement memory; JSON and JSONB
function families reject mismatched typed arguments. PostgreSQL 18
differential coverage crosses the former fixed boundaries.
The production-roadmap rebaseline found no externally blocked defect. Remaining
compatibility limits and operational work are tracked in PLAN.md.
The PostgreSQL comparison provenance audit found no externally blocked defect.
The harness now records the reference server's durability settings and storage
setup, and labels local object-store fixture timings as exploratory. A zero
replica count no longer enters the scale loop and launches unintended replicas.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
