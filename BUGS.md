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
The table-function row-width audit found no externally blocked defect.
XMLTABLE, JSON_TABLE, and publication-introspection rows now use statement
memory beyond 256 rows. The audit also fixed publication enumeration of the
internal large-object relation and XML `PASSING` syntax that accepted a cast
where PostgreSQL requires a primary expression while rejecting trailing
`BY REF` or `BY VALUE`. PostgreSQL 18 differential,
allocation-forbidden, and empty-cache recovery regressions cover the former
limit.
The measured PostgreSQL baseline audit found no externally blocked defect.
The harness now completes against a local PostgreSQL 18 server on macOS,
flushes worker statistics before reading access-path deltas, builds secondary
indexes after loading the fixture, and bounds explicit checkpoint pressure.
The 1,000-row scaling audit found no externally blocked defect. Completed
object reads now release fixed-slot pressure and no longer park writes with no
GET in flight; scans stream redundant spilled rows through the merged cursor.
The harness records cache size and query timeout, and all 59 workloads now
complete against actual PostgreSQL 18. The remaining checkpoint cost is
performance qualification work in PLAN.md.
The value-index checkpoint audit found no externally blocked defect. Its
rebuild now encodes keys and covering payloads from the merged spill cursor
instead of reopening each spilled row. A zero-cache regression qualifies
primary, covering, and posting indexes across updates, deletes, insertion,
and object-cold recovery. Remaining checkpoint interference is performance
qualification work in PLAN.md.
The published-block reuse audit found no externally blocked defect. A
checkpoint rebuild now reuses unchanged, typed blocks from the previously
published index roster; a zero-cache GIN regression covers write reduction
and recovery after an update. Remaining sort and cleanup traffic is tracked
as performance qualification in PLAN.md.
The bounded checkpoint-sort audit found no externally blocked defect. Small
value-index rebuilds now consume sorted rows from startup memory without
publishing temporary runs, while larger rebuilds retain the bounded external
merge. Zero-cache checkpoint writes, spill-path recovery, and the clean
focused benchmark qualify the change; remaining checkpoint work is in
PLAN.md.
The paired PostgreSQL checkpoint-baseline audit found no externally blocked
defect. The harness now verifies that both systems complete three explicit
checkpoints during the same mixed SQL workload, records PostgreSQL 18
durability and local storage, and preserves raw evidence. Larger and isolated
performance qualification remains in PLAN.md.
The checkpoint-profile audit found no externally blocked defect. A fixed
operation-count comparison could finish on PostgreSQL before all three
checkpoint commands completed, so the focused harness now runs for a minimum
duration and verifies the checkpoint count. Its fixed-memory phase log and
aligned object-store request window account for all measured cleanup DELETEs.
Foreground overlap and larger-scale qualification remain in PLAN.md.
The checkpoint-correlation audit found no externally blocked defect. The
maintenance thread could complete its three commands before foreground workers
left their barrier, so command count alone did not prove overlap. Maintenance
now starts after the worker barrier, traced client intervals must overlap a
profiled phase, and cross-process placement uses a common realtime axis because
Python's macOS monotonic clock and POSIX `CLOCK_MONOTONIC` have different
suspend behavior. Per-process durations remain monotonic. The resulting clean
run ranks publication and cleanup overlap in PLAN.md.
The final-slice publication audit found no externally blocked defect. A paced
sweep yielded after writing its last outdated table, allowing the next
foreground statement to invalidate that slice before the manifest beat. The
sweep now publishes while every captured generation is still current when no
merge beat is due. The full test suite exposed that unconditional same-beat
publication could starve alternating compaction until fixed checkpoint scratch
filled; the publication path now preserves that merge yield. The performance
harness also allowed automatic checkpoint work from the baseline to enter the
profile window, so each engine now completes an unmeasured settling checkpoint
before interference counters begin. A two-table regression proves the paced
and same-beat paths, the long-history regression proves bounded merge progress,
manifest retry remains idempotent, and object-cold recovery retains both
updates.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
