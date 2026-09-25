# Known bugs

This file records only defects that cannot be fixed in the current change
because an external blocker or genuine intractability prevents a fix. Fixable
defects belong in the implementation that discovers them; planned engineering
work and architecture limits belong in [PLAN.md](PLAN.md).

There are currently no defects that meet this file's inclusion criteria.
The resource-matched PostgreSQL baseline audit found no external blocker. Each
backend run now retains both an unconstrained host-available control and a
stock PostgreSQL 18 container capped to the CPU availability and exact fixed
memory plan recorded for pos3ql. The harness validates Docker's effective CPU,
memory, and no-swap limits instead of accepting an unverified comparison. The
clean 10,000-row matrix completed all 27 engine and scenario combinations
without error. Full mode now rejects fewer than four clients up front rather
than failing its mandatory group-commit amplification gate after partial work.
The object-store benchmark-matrix audit found no external blocker. The harness
now runs paired fixture, MinIO, SeaweedFS, and vanilla PostgreSQL 18 workloads,
records exact backend provenance, and represents unavailable provider request
counters explicitly. HTTP readiness closes a MinIO TCP-accept race, and
optional argument paths no longer rely on empty-array behavior that differs
between macOS and Linux Bash. The clean 10,000-row matrix completed every
backend and PostgreSQL workload without error; its differing generation shapes
remain an evidence constraint recorded in PLAN.md rather than an unresolved
defect.
The row-SST delta discovery audit found no external blocker. Exact unpublished
row identities remain in the startup-sized overlay until their table generation
publishes, so delta checkpoints avoid an immutable-table scan without relying
on row-byte residence. Cache-disabled reslice, pinned-snapshot merge, retry,
object-cold recovery, and storage VOPR regressions qualify the boundary. WAL
replay now reports configured overlay exhaustion instead of silently losing a
spilled-row delete marker.
Checkpoint value-index output pacing found no external blocker: the retained
fixed-memory sort source survives bounded writer beats and retry, while exact
table and binding identity prevents one relation's staged install from
suppressing the same binding ordinal on another relation, and exact dirty LSNs
reject stale work after foreground commits and `REINDEX`. A dirty binding keeps
the sweep active even when its row slice is already clean.
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
The final-slice CI audit also found no externally blocked defect in transaction
retry. A cold row needed for final WAL staging could park COMMIT after its
error path had rolled back and cleared the transaction. Cleanup retains the
numeric identity for diagnostics, so the generic retry path then mistook the
statement mark for live undo state and rewound a cleared deferred-trigger byte
buffer. Retryable waits now preserve the transaction and its locks through WAL
staging, and statement-mark ownership also requires an active transaction. A
direct boundary regression and the exact forced-spill differential corpus
cover the fix.
The checkpoint-deletion pacing audit found no externally blocked defect.
Commit pruning now runs as retryable post-publication maintenance, and the
configured per-beat object limit applies separately to commit, legacy SST, and
block namespaces. A one-object regression covers complete progress, retry,
explicit drain, and object-cold recovery. Remaining checkpoint publication
traffic and representative performance qualification are tracked in PLAN.md.
The selective value-index publication audit found no externally blocked
defect. Committed row changes now carry an allocation-free physical-column
footprint, and durable bindings record key, predicate, and included-column
dependencies. Unrelated updates retain published generations; row-set
replacement and replay paths invalidate conservatively. A WAL-only recovery
regression caught and closes the direct-replay invalidation gap, and the clean
profile plus actual PostgreSQL 18 comparison are recorded in PLAN.md.
The checkpoint row-reslice audit found no externally blocked defect. Compatible
stale slices now remain in the pending manifest and later slices include only
newer committed versions. Startup-bounded generation LSNs preserve warm reads
and guide later eviction, while relation replacements rebuild without a
reusable published base. Value-index work completes before the retained row
slice is advanced so an I/O retry cannot duplicate an interval. Row lists and
value-index installs are staged before discarding the retained slice, so a
failed object write retries with the same tombstone and LSN boundary. A focused
fault-injection regression, the storage VOPR corpus, checkpoint suite, clean
profile, and object-cold recovery qualify the change.
The 10,000-row checkpoint-scale audit found no externally blocked defect.
Checkpoint reslices now retain unchanged staged value-index generations, and
cold `ANALYZE`, `CREATE INDEX` validation, and value-cache population stream
the merged PAX cursor without per-row or per-column-range amplification. Full
container reads remain limited to dense column demand, so selective query and
startup cache paths preserve column pruning. The completed profile identifies
the full-roster row rewrite as the next bounded-dispatch target in PLAN.md.
The bounded full-roster audit found no externally blocked defect. Dirty filled
generation lists now free a slot through restartable pair-merge beats before
row publication. Completed merges use fixed startup storage per table, so
several filled tables can join one manifest publish without an unbounded
rewrite. Focused coverage interleaves foreground mutation across beats, injects
object-store failure, checks configured merge exhaustion, and verifies warm and
object-cold results. The repeated 10,000-row profile removed the 27.50-second
`row_sst_full` event; remaining value-index dispatch work is tracked in PLAN.md.
The paced value-index source audit found no externally blocked defect. Source
collection now retains its logical resident and merged-spill positions, paces
spill-generation initialization and row walking by object GETs, and decodes
only the active binding's physical PAX dependencies. External run generation
uses restartable fixed-memory binary carries. Qualification found and fixed a
deferred source row overwritten by carry-merge scratch and a detached run
cursor that treated an exact block-end resume as a truncated entry. Multi-run,
multi-block PAX, fault-injection, publication, and empty-cache recovery
regressions cover the fixes. The repeated 10,000-row profile had no four-PUT or
eight-GET schedule violation; durable row-format work is tracked in PLAN.md.
Storage VOPR fault qualification also found and fixed a cross-beat seam where
an uncommitted version could move a spill-resident committed row into the
overlay class without advancing the committed generation. Checkpoint sources
now partition rows solely by committed home and verify the spill commit LSN.
The durable format boundary audit found no external blocker. Manifest v13/v14
and row SST v2/v3/v4 compatibility are explicit typed identities; mixed row
generations retain their identity through all reader paths, new published row
generations use v4, and unknown formats fail at startup. Online generational
replacement and the offline gate for retiring a reader are documented in the
durable format contract.
The row checkpoint traffic audit found no external blocker. Compaction
scheduling now reads PAX descriptor metadata without fetching column extents,
and write beats stop on a provider-neutral object-read boundary after finishing
the current row. V4 row deltas compress and pack canonical groups instead of
publishing PAX descriptor and column containers. Mixed-format reads, exact
object counts, retry paths, and empty-cache recovery qualify the change. The
remaining row-merge write amplification is tracked in PLAN.md.
The row-merge read audit found no external blocker. Merge writers retain
decoded source groups in startup-accounted memory and use sparse-key seeks when
the schedule skips source blocks. Full-row PAX decoding reads a shared packed
container once and still verifies every logical frame; selective query readers
continue to fetch only demanded columns. A cache-disabled wide-row regression
bounds provider reads across paced beats, retry, and empty-cache recovery, and
the clean profile records the removed read amplification. Remaining immutable
output reuse is tracked in PLAN.md.
The row-merge output audit found no external blocker. Complete PAX groups that
survive merge pruning retain their immutable descriptor and column-container
references, and the merged roster keeps every shared physical dependency live.
Groups affected by duplicate selection, snapshot pruning, or removable
tombstones rebuild normally. A cache-disabled regression combines both paths,
runs garbage collection, and reads reused payloads after empty-cache recovery.
The clean profile records the reduced PUT traffic and exposed value-index
schedule work addressed by the following audit.
The incremental value-index audit found no external blocker. Checkpoint sorting
now contains only resident rows newer than the published index LSN and merges
them with a fixed-memory ordered stream of ordinary roster or navigation-tree
entries. A startup-bounded identity set captured with the delta supplies exact
base suppression across later commits and writer retries. It covers key moves,
predicate exits, deletes, covering payloads, and posting tokens without
accumulating stale generations. Relation rewrites and catalog maintenance
retain explicit full rebuild state. Fault retry, per-beat object limits,
garbage collection, and object-cold navigation recovery qualify the path. The
complete storage VOPR range found that live-LSN suppression could lose an
unchanged base entry after a non-indexed commit, while rollback could leave a
redundant spilled overlay state that shadowed an immutable index candidate.
Exact captured identities and rollback cleanup close both classes; seeds
460259 through 460274 cover their outage and restart variants. Validation also
caught an unsafe row-SST delta shortcut: a changed row can spill before the
checkpoint, so scanning only the resident map lost that row after cold
recovery. Row-SST delta discovery retains the complete logical scan.
The external benchmark harness audit found no product defect. The new profile
rejects plaintext or ambiguously operated services, isolates every run under a
recorded object prefix, records the hardware, network, cache, and PostgreSQL
storage conditions needed to interpret a representative comparison, and keeps
credentials out of artifacts. Unit coverage validates the provenance contract
and report wording. The representative long-running run remains an explicit
PLAN.md infrastructure task rather than a software bug.
The large-catalog benchmark audit found no product defect. The suite now builds
a bounded, configurable relation catalog, verifies its complete `pg_class`
shape, and measures exact name and OID lookup before and after empty-local-cache
recovery. PostgreSQL controls receive the same setup and warm workload. The
128-relation smoke and reduced full PostgreSQL-control qualification completed
without errors; representative timing remains part of the external run tracked
in PLAN.md.
The routine-width audit found and fixed a coupled 64-column table-function
descriptor that could panic after widening stored routine metadata. Callable
input signatures now accept PostgreSQL's exact 100 arguments, result metadata
uses the independent 1,664-column executable tuple boundary, and the capacities have
distinct fixed arrays. The accepted maxima and over-limit errors are qualified
through execution, catalog output, allocation-forbidden WAL encoding, journal
replay, checkpoints, object-cold recovery, and PostgreSQL 18 differential
execution.
The prepared-parameter audit found no external blocker. Wire parameter counts
now decode across the complete unsigned 16-bit range, prepared type metadata
shares each startup-sized statement buffer, and portals retain compact Bind
metadata instead of compiled span and format arrays. SQL PREPARE type codes use
the same bounded storage, while inference and decoded values use exact
statement-arena slices. Allocation-forbidden maximum-width Parse, Bind,
Statement Describe, Execute, SQL PREPARE, catalog, and over-limit tests pass,
and a raw PostgreSQL 18.6 differential qualifies all 65,535 parameters.
The integration audit also corrected the connection memory plan to count both
prepared pools, retained enough prepared slots for `pg_dump`, and replaced the
withdrawn MinIO image with a provenance-pinned Alpine rebuild whose bucket is
created by a signed S3 request.
The grouping-width audit found no external blocker. Grouping expressions and
sets now use variable-width arena bitmaps at PostgreSQL 18's exact 1,664-entry,
4,096-set, 12-element `CUBE`, and 31-argument `GROUPING()` boundaries. The
maximum-set execution test exposed retained per-set scratch that exhausted the
statement arena; completed encoded rows now move to the persistent tail and
front scratch is recycled between sets. Allocation-forbidden maximum and
over-limit execution plus a raw PostgreSQL 18.6 differential qualify the fix.
The result-column audit found no external blocker. Query projection and wire
metadata now accept PostgreSQL 18's 1,664-column target-list boundary. The
server and query-bearing test workers use one shared, fixed 128 MiB startup
stack envelope for the statically bounded scratch. Simple, scoped, and set
execution, Statement Describe, alternating per-column text and binary Bind
formats, and the exact 1,665-entry rejection are qualified allocation-free and
against PostgreSQL 18.6.
The join-capacity audit found no external blocker. Parser, scope, executor,
materialization, subquery, dependency, catalog, and plan state now size range
tables and accumulated `USING` contributors from the statement arena. Compact
64-bit PAX-demand, reorder, and parameterized-index proofs remain optional
optimizations; wider joins execute exactly in identity order with full-row
decoding. Allocation-forbidden and PostgreSQL 18.6 differential coverage
qualifies 128 relations through cross and `USING` joins, plans, stored views,
windows, materialization, subqueries, and joined DML. Explicit `USING` lists
now follow the 1,600-column relation boundary.
The multirange-width audit found no external blocker. Component readers now
stream from the canonical value, while canonicalization, constructors, and set
operations use exact statement-arena slices and rendered output uses exact
arena bytes. Allocation-forbidden 128-component execution, binary Bind/result
and COPY comparison with PostgreSQL 18.6, indexes, WAL, checkpoints, and
object-cold recovery cross the former 64-component and 1 KiB limits. The audit
also found that coverage instrumentation plus half of the growing SQL corpus
could exceed its worker limit; the corpus now has three guarded
shards while auxiliary probes retain their independent worker. That split
exposed dropped partition descendants whose stored parent ordinal could match a
new relation after catalog-slot reuse. Partition ancestry now requires every
node to be transaction-visible, and a focused reuse regression covers recursive
index creation against the new parent.
The relation-width audit found no external blocker. Stored relation, view,
named-composite, and record-definition shapes now accept 1,600 columns, while
executable table-function tuples accept 1,664. The audit replaced one-word
durable column masks, widened counts and ordinals with backward readers, moved
transient record shapes into statement memory, and streamed composite
checkpoint records directly into the reserved manifest. It also separated
physical full-row decoding from exact high-column authorization after a
column-level privilege regression exposed the coupling. Allocation-forbidden
boundary execution, WAL replay, checkpoint publication, object-cold recovery,
and a raw PostgreSQL 18.6 differential qualify the accepted and rejected
widths. CI then exposed a recursive-CTE regression: every iteration rebuilt a
full 1,664-slot relation definition and exhausted the default statement arena
at 100 one-column rows. Materialized recursive relations now retain one typed
definition, including the recursive reference's column aliases. Focused 32 MiB
regressions cover both plain and aliased recursion. The growing library,
curated differential, and seeded fuzz suites now run as guarded deterministic
shards under the unchanged 15-minute job ceiling. Those shards exposed another
width-coupling regression: scalar projection inspected set-returning routine
candidates for every row by copying the complete widened routine definition.
Candidate probes now read the transaction-visible kind, attributes, signature,
and defaults in place, and projections without a set-returning call skip the
per-row materialization scratch. The PostgreSQL 18.6 differential covering
1,100 mutable calls and a 70,000-row scroll cursor fell from a timeout to a
bounded passing run. Coverage also exposed stack-sensitive pgoutput decoding:
decoded tuple and relation messages embedded complete 1,600-column arrays.
They now retain validated borrowed wire views and iterate without allocation,
including at the maximum width. Instrumented exact, binary COPY, type,
PostgreSQL regression, and sqllogictest phases now have independent workers.
The deterministic fuzz sequence likewise runs in four 2,500-statement quarters
after a 5,000-statement half matched PostgreSQL completely but reached the job
ceiling during teardown.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
