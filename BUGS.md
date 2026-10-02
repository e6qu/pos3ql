# Known bugs

This file records only defects that cannot be fixed in the current change
because an external blocker or genuine intractability prevents a fix. Fixable
defects belong in the implementation that discovers them; planned engineering
work and architecture limits belong in [PLAN.md](PLAN.md).

There are currently no defects that meet this file's inclusion criteria.
The operational-interface audit found no external blocker. A separately
bounded listener now serves liveness, writer-fence-validated durable readiness,
Prometheus metrics, and JSON capacity without consuming PostgreSQL connection
slots or allocating after startup. JSON Lines logging shares the allocation-free
runtime diagnostic path. Strict configuration and HTTP parsing, fixed response bounds,
and the controlled-replacement runbook define the operator boundary. A small
valid configuration exposed that the memory plan omitted the fixed LISTEN and
NOTIFY registry and outbox; their exact bytes are now charged before startup.
Active failure detection, credential rotation, packaging, and end-to-end recovery
drills remain planned work in PLAN.md.
The single-writer fencing audit found no external blocker. A random process
incarnation is promoted through one durable fence; the transition precedes
conditional rewrites of both mutable roots, and activation follows them. A
root conflict can be adopted or retried only after revalidating the active
fence. Tests cover a displaced writer, delayed requests carrying both stale
root ETags, successor publication, interrupted promotion recovery, and another
restart race. Restore and export publish their restart markers before
promotion and verify ownership before each live-root replacement.
The point-in-time recovery audit found no external blocker. Current commit
records carry PostgreSQL-epoch commit time while legacy forms remain readable.
Recovery validates that the live descriptor chain descends from the named
backup, verifies immutable batch checksums and WAL framing, and publishes only
a whole-transaction head. A target within one uploaded batch gets a checksummed
prefix with the original predecessor. Invalid LSNs fail before the durable
marker; interruption, retry, timestamp and exact-LSN selection, empty local
caches, deletion, and a new durable branch are covered.
The independent-prefix backup-export audit found no external blocker. Export
uses the provider-neutral object contract for both configurations, copies in a
fixed namespace batch and fixed object buffer, and adopts only byte-identical
immutable objects on retry. The audit caught backup-root reads that assumed a
whole manifest fit one response and an omitted durable extension-package
namespace; ranged root reads and complete immutable namespace copying close
both gaps. Durable pending and completion records block partial startup and
make a lost final response adoptable; interruption after a copied object,
retry, a bounded forward namespace scan, a multi-range copy, complete source
loss, durable extension execution, backup deletion, a new destination branch,
and empty-cache recovery are covered.
The named-backup audit found no external blocker. Backup manifests now pin
their complete row and value-index block graphs; prepared-transaction and
logical-slot positions lower the oldest backup replay floor used by commit
pruning. A checksummed completion record, durable restore marker,
local-cache removal, later checkpoints, deletion, and a new post-restore branch
are covered through empty-cache recovery. Independent-prefix export and
between-checkpoint recovery targets are now part of the implemented baseline.
The durable-format contract audit found no external blocker. Manifest and row
SST reader and writer identities now come from complete executable sets whose
membership is checked against the documented matrix. An actual v13 manifest
recovers from empty caches, upgrades through a v14 checkpoint, and recovers
again from empty caches, closing the former parser-only migration evidence.
The durable constraint-shape audit found no fixable defect within manifest v14.
Table and domain constraint positions are stable catalog and referential-trigger
OID identities, so widening their 64-entry stride needs the format and OID
migration required by PLAN.md. Duplicate OID arithmetic now uses shared
constructors, and accepted, rejected, checkpoint, and object-cold boundaries
remain covered.
The geometric-value width audit found no external blocker. Path and polygon
parsing, generated values, operators, binary wire bodies, and spatial index
summaries now use exact statement memory or streaming traversal. A 300-point
boundary is covered under allocation-forbidden execution, SQL differential
comparison, GiST lookup, checkpoint publication, and empty-cache recovery.
The enum-label capacity audit found no external blocker. Enum member storage is
now reserved from `max_enum_labels_per_type` for committed, transactional, and
recovery images; savepoint rollback, unsafe new-value tracking, widened WAL,
legacy WAL reading, checkpoint streaming, allocation-forbidden execution,
database cloning, and empty-cache recovery are covered beyond the former
64-label boundary.
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
The deterministic fuzz sequence likewise runs in five 2,000-statement slices.
The former 2,500-statement final slice reached the unchanged job ceiling before
reporting its summary; the smaller slices preserve all 10,000 statements.
The forced-spill corpus now runs in six complete file slices. Five slices were
no longer enough after the constraint-width corpus joined the suite; the old
final slice reached the same ceiling without completing.
The policy-role width audit found no external blocker. Policy targets now use
startup-sized dense images bounded by the configured role catalog plus
`PUBLIC`, including transaction-local ALTER versions and recovery scratch. WAL
uses a new wide-count record while retaining the old reader, and checkpoint
records stream role names without a fixed intermediate buffer. PostgreSQL 18.6
differential coverage and allocation-forbidden catalog and enforcement checks
cross the former 64-role boundary; journal replay, rollback, checkpoint, and
object-cold recovery preserve the widened list. The wider PostgreSQL comparison
also exposed `pg_policies.roles` insertion ordering; the view now sorts role
names through allocation-free statement scratch like PostgreSQL's `pg_authid`
subquery. The widened guarded catalog query also exposed scalar subqueries
publishing their single buffered row as a temporary object. They now consume
that row directly from the startup-sized external-sort chunk. Memory-plan
regression coverage accounts for the role images alongside policy definitions
and stored-query dependencies.
The credential-rotation and release-package audit found no external blocker.
Object-store credentials now reload from an owner-only bounded file only after
the candidate renews the current writer fence; failures retain the installed
credential and make readiness, logs, and metrics explicit. The external suite
rotates through rejection, old-credential revocation, durable checkpoint work,
and SIGHUP restoration. Tagged Linux archives are checksummed and exercised
from extraction through startup, liveness, and graceful shutdown in CI.
The full-text value-width audit found no external blocker. Vector lexemes,
positions, query nodes, ranking operands, headline words, and set-returning
state now use statement memory, while binary output and index folding stream
canonical values. The audit preserves PostgreSQL's distinct text and binary
lexeme limits and per-lexeme position truncation. Allocation-forbidden wide
execution, binary round trips, GIN and GiST indexes, checkpoint publication,
empty-cache recovery, and PostgreSQL 18.6 differential coverage cross the
former 512-item and 2,048-total-position limits. Forced-spill coverage also
unnests the durable vector through the external lateral path.
The automatic-failover audit found no external blocker. The S3 fixture's
content-derived ETags exposed mutable-root promotion rewriting identical bytes,
which let a displaced process retain a usable commit-head precondition until
the candidate published again. Promotion now embeds the new process token in
both roots before activation. The packaged zero-cache configuration also
exposed restartable asynchronous reads repeatedly consuming and refetching the
same completed block; a cacheless stack now uses synchronous provider reads.
The token-bearing empty manifest retains the existing 64-byte minimum
manifest capacity through its compact writer field.
The release-package scenario pauses a primary, automatically starts a
candidate, verifies empty-cache recovery, resumes the old connected session,
and observes SQLSTATE `40001` before clean shutdown.
The query-workspace audit found no external blocker. The former single arena is
now an exact startup-sized slot set with exclusive dispatcher selection; zero
and over-connection configurations fail before startup. Allocation-free slot
isolation, memory-plan charging, and operational capacity reporting are covered.
The COPY concurrency audit found no external blocker. Streamed statement
transition rows formerly lived in one engine-global buffer, so interleaved
clients could clear or mix the rows seen by AFTER STATEMENT triggers. Each
connection and logical subscription worker now owns an exactly charged fixed
buffer, with direct engine and real-driver interleaving regressions.
Coverage also exposed a raw-wire catalog probe whose single oversized request
could exceed the socket response deadline under instrumentation. Its object
setup, comments, reads, and cleanup now use bounded protocol exchanges.
The ordinary DML workspace audit found the mutable physical-row selection
buffer was still engine global. Client execution now selects an independently
allocated, exactly charged buffer with the same exclusive lease as its query
arena. The follow-up dispatch audit replaced collision-prone connection modulo
mapping with a startup-bounded owner and FIFO-waiter roster. Release,
disconnect, and reactor-interest failure hand the exact slot to the oldest live
waiter without allowing two owners to alias it.
Logical subscription apply and bootstrap COPY use their worker-owned arena and
buffer. The backend-identity audit also removed Storage's shared mutable
connection selector. Each leased workspace retains its connection identity and
publishes it in fixed thread-local execution context; isolation and exact
memory accounting are covered across workspace and operating-system threads.
The database-selector audit extended that boundary through catalog access and
WAL staging. It also found that a database without its required public schema
could leave a failed selection active; failure now restores the prior database.
The workspace-selector audit found that the arena and DML sets still carried
one shared active index after their session identities became worker private.
Both now resolve one typed thread-local lease identity; slot and operating-system
thread isolation cover arena, DML scratch, backend, and database selection.
The dispatch-completion audit found no typed boundary between engine return and
reactor response handling. Readable dispatch and parked retry now return one
completion with the exact lease and a snapshot of backend and database
identity. The reactor validates and releases that lease at engine completion,
preserving group publication width, then restores the snapshot before later
response cleanup. The audit also found that response failure, disconnect, and
cross-session cancellation could inherit the most recently dispatched
session's thread-local context. Connection release and cancellation now restore
the target identity at their choke points. Parked retries use the same typed
completion path.
The dispatch-queue audit found no bounded handoff between reactor readiness and
engine execution. Readable work and parked retries now share an exactly charged
FIFO sized to the workspace count. A queued dispatch owns its workspace and a
connection-slot in-flight bit until completion; read interest is suspended so
separate kqueue read and write events cannot process or close the same slot
while its job is queued. FIFO wraparound, allocation-free operation, and exact
memory accounting are covered. Performance qualification found that initially
draining only at the end of a reactor turn capped group commit width at the
workspace count and amplified concurrent commit PUTs. Capacity pressure now
drains queued scheduler chunks while retaining their responses for the turn's
single publication barrier.
The engine-transfer audit found five compiler-enforced blockers: reference-
counted durable and temporary block stores, the simulated namespace, immutable
external-run readers, and the owned POSIX locale handle. The stores and readers
now use synchronized shared ownership, lock poison fails loudly instead of
masquerading as pool exhaustion, the simulator exposes a transferable handle,
and its registry is process-wide so a later client opened on another thread
resolves the same namespace. Locale transfer is confined to its owned wrapper.
A live engine now moves to another operating-system thread, executes SQL, and
drops there in the test suite. This establishes single-owner transfer; shared
engine mutation remains the next fixed-worker boundary in PLAN.md.
The search-path audit found that Storage still held one mutable effective path
for all name resolution. Concurrent workers could therefore redirect each
other between schemas. The effective path now lives in fixed thread-local
execution state, is published at each statement boundary, and is swapped and
restored locally for nested view and schema execution. Cross-thread isolation,
ordinary path resolution, durable stored-query identity, and live engine
transfer retain direct coverage.
The transaction-visibility audit found that Storage also carried one mutable
command snapshot and one durable commit snapshot. A worker lowering visibility
for a data-modifying common table expression or pinning a repeatable-read
statement could therefore change the row, SST, and durable index generations
seen by another worker. Both snapshots now form one fixed thread-local
visibility context initialized and published at statement boundaries.
Cross-thread isolation, command-history visibility, repeatable reads, and cold
recovery retain direct coverage.
The authorization audit found one engine-global role-graph bitmap reused by
all membership and privilege checks. Concurrent workers could collide on its
mutable borrow or overwrite a traversal in progress. Each leased query
workspace now owns an independently allocated bitmap selected by the typed
worker-local identity. Exact memory-plan charging, workspace isolation, role
membership, object privileges, and column privileges retain direct coverage.
The foreign-statement audit found one engine-global transaction identity,
isolation flag, and savepoint roster published before remote execution. A
worker could therefore replace the context another worker was about to consume.
Each leased query workspace now owns an independently allocated context and
fixed savepoint roster selected by the typed worker-local identity. Exact
memory-plan charging and workspace isolation retain direct coverage. Transport
ownership was audited separately below.
The foreign-session audit found that the single transport slot rejected a
second local transaction or remote endpoint even when startup memory could
bound both. Foreign transports now use an explicit `max_foreign_sessions` pool
keyed by local transaction and endpoint. Every slot reserves its client buffers
at startup, reuse validates the typed owner, exhaustion returns SQLSTATE
`53300`, and transaction savepoint, rollback, and commit commands cover all of
the owner's remote sessions. Exact memory-plan charging, endpoint isolation,
slot reuse, operational capacity reporting, and live two-endpoint transaction
behavior retain direct coverage.
The coverage gate also exposed that the 4,096 grouping-set capacity probe could
outlast its 60-second socket deadline under instrumentation. The wide-capacity
probe now uses the existing 120-second deadline class and reports the active
case on timeout, while the workflow's 15-minute outer bound remains unchanged.
The cumulative-statistics audit found seven independently borrowed vectors and
shared reset timestamps that would panic or lose updates under overlapping
workers. They now share one startup-bounded state protected by a mutex.
Transaction finalization updates relation and database totals atomically, and
function counter creation and increment occur under the same lock. A four-worker
regression verifies synchronization and exact retained capacities.
The foreign-transport ownership audit found that each startup-bounded client
and its session record still used `RefCell`, so overlapping workers would panic
instead of serializing socket access. Each complete slot now uses a mutex and a
typed guard that retains ownership through remote activation. Reservation and
release share a pool assignment lock, preventing concurrent scans from
duplicating a transaction/endpoint session or claiming the same vacant slot.
The pool is compiler-checked as `Send + Sync`; endpoint isolation, typed lease
validation, exhaustion, reuse, and exact startup charging retain direct
coverage.
The transaction-identity audit found active identities, recent completion
statuses, their replacement cursor, and the latest observed identity split
across independent `RefCell` and `Cell` values. Overlapping workers could panic,
lose begin/finish updates, or construct a snapshot and checkpoint manifest from
different registry moments. One startup-bounded mutex now owns the complete
state. Identity transitions, status queries, snapshot construction, and
manifest traversal use one coherent lock boundary. A four-worker regression
and a `Send + Sync` assertion cover concurrent updates and exact retained
capacities.
The shared-lock audit found relation, row, and advisory lock state split across
three `RefCell` values while their wait graph and acquisition sequence formed
one consistency domain. Overlapping workers could panic on nested borrows, lose
sequence updates, or observe a partial savepoint, prepare, release, or wait-edge
transition. One startup-bounded mutex now owns all three registries and the
sequence. A four-worker regression and a `Send + Sync` assertion cover atomic
mutation and exact retained relation-lock capacity; existing functional suites
retain relation, row, advisory, deadlock, rollback, and prepared-lock coverage.
The backend-state audit found live activity and pending signal queues in
separate `RefCell` registries. Overlapping statement transitions, activity
views, LISTEN changes, disconnect, and signal processing could panic, while a
signal target could change between lookup and queue publication. One
startup-bounded mutex now owns both registries, and signal publication
revalidates the live process identity inside that state. A four-worker
regression and `Send + Sync` assertion cover concurrent mutation and both exact
retained capacities.
The transaction-snapshot audit found repeatable-read retention in an
unsynchronized vector and serializable table-generation reads in a `RefCell`.
Overlapping workers could race history-retention decisions or panic while
recording and validating serializable reads. One startup-bounded mutex now owns
both registries; schema blocker inspection observes snapshot and relation-lock
state in a fixed lock order. A four-worker regression and `Send + Sync`
assertion cover concurrent mutation and exact retained capacities.
The BRIN-maintenance audit found per-index metadata and unsummarized ranges in
separate `RefCell` pools. Overlapping discovery, summarize, desummarize,
rebuild, recovery, WAL, and checkpoint work could panic or pair metadata with a
different range image. One startup-bounded mutex now owns both pools, and every
mutation holds it across the complete state transition. WAL and checkpoint
callers copy one coherent fixed-size image. A four-worker regression and
`Send + Sync` assertion cover concurrent mutation and exact retained
capacities.
The sequence-value audit found committed and staged last-value, called,
prelog-count, and dirty fields stored in eight independent `Cell`s per catalog
entry. Concurrent `nextval`, `setval`, restart, rollback, replay, WAL,
checkpoint, and catalog reads could lose updates or combine fields from
different transitions. One startup-bounded mutex now owns complete committed
and staged images for every configured sequence. A four-worker reservation
regression also proves that publishing an older generation cannot clear a
later advance; `Send + Sync` assertions cover atomic mutation and exact retained
capacity.
The transaction-workspace audit found each foreign statement context and the
temporary-object transaction registry behind `RefCell`. Overlapping workers
could panic while publishing foreign transaction identity, isolation mode, or
savepoints, and temporary relation use could collide with PREPARE eligibility
or cleanup. Each fixed query workspace now owns a mutex-protected foreign
context, while one startup-bounded mutex owns the temporary transaction roster.
Allocation-free savepoint guards, four-worker mutation, `Send + Sync`, and exact
retained capacities have direct coverage.
The catalog-graph scratch audit found role reachability bitmaps and domain base
rebinding markers behind `RefCell`. A duplicated workspace lease or overlapping
catalog traversal could panic on a mutable borrow, and the containers prevented
the scratch boundary from being `Sync`. Each fixed query workspace now owns a
mutex-protected role bitmap, while domain rebinding uses one mutex-protected
fixed marker vector. Four-worker mutation, `Send + Sync`, and exact retained
capacities have direct coverage.
The reader-scratch audit found locale comparison buffers and the spill reader's
row buffers, merged-scan contexts, cursor rosters, value-index buffers,
external sorters, and walk identifier behind `RefCell` or `Cell`. Overlapping
workers could panic on mutable borrows, reuse a live buffer, or duplicate a
walk identity. The immutable locale and every fixed reader pool now have
mutex-protected ownership; pool exhaustion remains a named error. Four-worker
mutation, exact memory charging and capacities, and `Send + Sync` assertions
have direct coverage.
The index-expression scratch audit found the shared parser and evaluator arena
using an unsynchronized allocation frontier. Overlapping index maintenance or
rebuild work could race allocations and rewinds, while the arena prevented the
complete `Storage` state from being `Sync`. One mutex now retains the arena
through each nested parse, predicate, key, and spilled-row evaluation chain.
Four-worker mutation, exact retained capacity, and a `Storage: Send + Sync`
assertion have direct coverage.
The catalog-identity audit found every catalog family incrementing or restoring
one plain `catalog_seq` value. Independent catalog synchronization would let
concurrent creates lose increments and publish the same `created_at` identity,
while recovery could move the sequence backward relative to live allocation.
One atomic monotonic sequence now reserves every new identity and observes
every restored identity. Bounded trigger and index generations use the same
compare-and-exchange boundary and retain SQLSTATE `54000` at exhaustion.
Four-worker uniqueness, concurrent recovery observation, bounded exhaustion,
and `Send + Sync` have direct coverage.
The large-object catalog audit found the fixed definition pool and automatic
OID frontier in separate unsynchronized fields. Overlapping create, drop,
ownership, recovery, checkpoint, ACL, and catalog reads could lose OID advances
or combine a definition with another transition. One startup-bounded mutex now
owns both fields. Iterators copy each entry and release the guard before nested
catalog lookups, preventing recursive-lock stalls. `pg_largeobject_metadata`
uses the fixed configured roster instead of a racy count-then-fill pass. Four-worker allocation,
nested-reader progress, exact retained capacity, and `Send + Sync` have direct
coverage. The same audit found and fixed role dependency checks omitting owned
large objects, which could otherwise allow `DROP ROLE` to orphan their owner.
The object-comment catalog audit found committed text, transaction-private text
and identity overlays, and reusable slots in one unsynchronized fixed vector.
Overlapping COMMENT, rename, drop, replay, clone, checkpoint, and catalog reads
could lose a slot update or observe parts of different entry images. One
startup-bounded mutex now owns the complete catalog. Iterators copy one entry
and release the lock before resolving referenced objects, preventing recursive
lock stalls. Four-worker publication, nested-reader progress, exact configured
capacity, and `Send + Sync` have direct coverage.
The ACL catalog audit found object, column, default, and parameter privilege
entries in four unsynchronized fixed vectors. Overlapping GRANT, REVOKE,
commit, rollback, replay, clone, role cleanup, checkpoint, privilege, and
catalog operations could lose updates or combine fields from different entry
images. One startup-bounded mutex now owns all four catalogs. Iterators copy
one entry and release the lock before nested role or object resolution,
preventing recursive lock stalls. Four-worker publication across every ACL
family, nested-reader progress, exact retained capacities, loud exhaustion,
and `Send + Sync` have direct coverage. The same audit fixed recovery role
cleanup omitting column and parameter ACL entries.
The role catalog audit found role definitions, memberships, and per-role
settings in three unsynchronized fixed vectors. Overlapping role DDL,
membership or setting changes, replay, checkpoint, catalog, and authorization
work could lose updates or combine fields from different transitions. One
startup-bounded mutex now owns all three families. Iterators copy one entry and
release the lock before nested role, ACL, or catalog resolution. Four-worker
publication, nested-reader progress, exact retained capacities, loud
exhaustion, and `Send + Sync` have direct coverage. The same audit fixed
recovery role removal retaining memberships and settings that referenced the
reusable role slot.
The operator catalog audit found access method definitions, operators,
operator families, and operator classes in four unsynchronized fixed vectors.
Overlapping DDL, replay, database cloning, schema rename, checkpoint, overload
resolution, and dependency validation could lose updates or combine fields
from different transitions. One startup-bounded mutex now owns all four
families. Iterators copy one entry and release the lock before nested routine,
type, schema, role, or operator resolution. Four-worker publication,
nested-reader progress, exact retained capacities, access-method exhaustion,
and `Send + Sync` have direct coverage. The same audit fixed access-method drop
retaining comments keyed by a reusable catalog identity.
The type catalog audit found domains, enums and their committed and pending
member images, and named composites in five unsynchronized fixed vectors.
Overlapping type DDL, replay, database cloning, schema and type moves,
ownership, checkpoint, and catalog work could lose updates or combine fields
from different transitions. One startup-bounded mutex now owns the complete
type namespace. Iterators copy one fixed definition or member and release the
lock before nested catalog resolution. Four-worker publication across every
type family, nested-reader progress, exact retained capacities, domain
exhaustion, and `Send + Sync` have direct coverage.
The schema-bound text catalog audit found collations, conversions, and text
search objects in three unsynchronized fixed vectors. Overlapping DDL, replay,
database cloning, schema rename, checkpoint, dependency, catalog, and
collation-execution work could lose updates or combine fields from different
transitions. One startup-bounded mutex now owns all three families. Iterators
copy one fixed definition and release the lock before nested schema, comment,
dependency, or execution lookup. Four-worker publication, nested-reader
progress, exact retained capacities, collation exhaustion, and `Send + Sync`
have direct coverage. Database removal also clears transaction-private pending
definitions before their slots are reused.
The routine catalog audit found complete routine definitions, pending identity
and replacement images, and ownership in an unsynchronized fixed vector.
Overlapping DDL, replay, database lifecycle, schema or type rewrites, overload
resolution, dependency rebinding, checkpoint, and catalog work could lose
updates or combine fields from different definitions. One startup-bounded
mutex now owns the catalog. Readers copy one fixed definition and release the
lock before nested type, role, ACL, comment, dependency, or execution lookup.
Four-worker publication, nested-reader progress, exact retained capacity,
routine exhaustion, and `Send + Sync` have direct coverage. Failed creation
and committed or rolled-back removal now clear the complete reusable slot.
Replacement replay normalizes the complete WAL image to committed catalog
state before publishing it, so its transaction-private create marker cannot
hide an otherwise recovered routine. The complete library suite now runs in
four deterministic slices after a 559-test slice exceeded the CI worker's
15-minute ceiling. The 10,000-statement differential sequence now runs in ten
1,000-statement slices after a 2,000-statement slice passed its assertions but
reached the same ceiling during runner cleanup. The execution-width corpus now
has its own worker after its 70,000-row cursor began too late in a general
corpus slice to finish within the same bound. Sqllogictest read-only queries now
run in eight slices after a four-slice worker spent 11 minutes 41 seconds in
replay and reached the ceiling before reporting its result.
Instrumented sqllogictest queries now run in four slices after a two-slice
worker passed the complete differential suite but reached the ceiling during
runner cleanup. The uninstrumented auxiliary differential now runs PostgreSQL
regression, exact errors, COPY, type fidelity, LISTEN/NOTIFY, and binary
composites independently after their combined worker completed the regression
and protocol phases but reached the ceiling during binary composites.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
