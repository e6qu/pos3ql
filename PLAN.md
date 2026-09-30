# pos3ql roadmap

Architecture: [README.md](README.md). Naming: [docs/terminology.md](docs/terminology.md). Working rules: [AGENTS.md](AGENTS.md). Externally blocked defects only: [BUGS.md](BUGS.md).

## Product boundary

pos3ql is a PostgreSQL-compatible database whose durable state lives in object
storage. RAM and local disk are bounded, disposable caches.

- Compatibility is defined at PostgreSQL SQL text, v3 wire, catalog, tool, and
  logical-replication boundaries. Unsupported behavior must fail explicitly.
- Durability uses immutable commit batches and checkpoint SSTs published by
  compare-and-swap. PostgreSQL heap pages, physical XLOG, physical streaming
  replication, and binary-WAL tooling are not targets.
- One direct S3-compatible HTTP data-plane implementation serves every object
  store. No provider SDK, custom storage service, translation proxy, or
  provider-specific branch is permitted. The versioned protocol profile and
  multi-implementation qualification suite are the portability boundary.
- Runtime memory is fixed at startup. Execution, caching, sorting, background
  work, and concurrency must remain within named pools and fail loudly on
  exhaustion.
- Third-party PostgreSQL extensions, whether SQL-only or native, are not
  compatibility targets. C shared libraries, the PostgreSQL server ABI, hooks,
  and background workers are also out of scope. The already implemented SQL
  extension package lifecycle remains part of the accepted SQL surface, but no
  further extension qualification or ecosystem work is planned.

## Current state

The [implemented baseline](docs/implemented-baseline.md) records completed
capabilities and their qualification. The [PostgreSQL 18 compatibility matrix](docs/postgresql-18-compatibility.md)
records the accepted SQL and catalog surface; [index navigation](docs/index-navigation.md)
records the physical access paths and the query shapes that retain exact full
scans. The [performance boundary](docs/performance.md) describes the current
single-process topology and measurement harness.

The engine publishes object-native commits and checkpoints, recovers with empty
local caches, and supports a broad PostgreSQL SQL, wire, catalog, tool, and
logical-replication surface. Runtime memory is fixed at startup. Recent capacity
work moved catalogs, transaction and checkpoint bookkeeping, stored programs,
statement lists, arrays, and JSON values and paths to their declared memory or
durable-format bounds. The principal specialized-index predicate and finite,
unfiltered geometric nearest-neighbor workloads have bounded immutable-object
navigation with warm and object-cold qualification. Predicates that cannot be
pruned conservatively still use complete exact evaluation.

The durable-format contract declares the complete readable and writable
manifest and row SST identity sets in code and documentation. Manifest v13
recovers through empty caches and upgrades to v14 on the next checkpoint;
mixed v2, v3, and v4 row generations remain readable. Unknown identities stop
startup, and an incompatible change requires the offline migration procedure
defined by the contract before its writer can ship.

Offline named backups now retain checksummed manifest and commit-head roots in
the configured object-store prefix. Garbage collection includes every backup
block graph and keeps commit history from the oldest required backup replay
floor. Restore uses a durable pending marker, clears local caches, and can
branch new durable history from the restored checkpoint. The fixed
`max_backups` roster bounds retention bookkeeping. A restartable export copies
a named point and its immutable block, commit, and extension-package objects
into an empty independently configured prefix, publishes it as that prefix's
live state, and recovers after complete source loss. LSN and timestamp recovery
derive a validated whole-transaction head from retained history, resume through
a target-bound durable marker, recover with empty local caches, and permit a
new durable branch after backup deletion.

Single-writer ownership now uses a durable process-incarnation fence shared by
commit-head and manifest publication. Startup promotes a fresh random token by
first transitioning ownership, then retagging both mutable roots with that
token, and only then becoming active. This changes content-derived ETags as
well as version-derived ETags. The displaced process fails its next publication
with SQLSTATE `40001`; delayed root requests and interrupted or competing
restarts are covered. Legacy unfenced prefixes promote in place.

This is not yet a production-complete topology: one process serializes query
execution, and representative long-run performance evidence remains open. A
separately bounded operational listener exposes health, readiness, metrics, and
capacity, with structured logs and a controlled-replacement runbook. Release
packages include a single-authority passive-candidate monitor for automatic
failure detection and promotion. Object-store credentials rotate through a
validated owner-only file, and tagged releases produce an install-tested Linux
archive with checksums and service files.

## Remaining production work

### Compatibility and capacity

Maintain a capacity inventory that distinguishes PostgreSQL protocol or type
bounds, durable-format bounds, configurable startup capacities, statement-memory
bounds, and narrower implementation limits. For each narrower limit, record the
accepted shape, SQL or wire error, storage representation, and the reason for
keeping or lifting it. Explicit rejection protects correctness but does not
make a smaller accepted surface PostgreSQL-compatible at that width.

The audited durable 64-item constraint definition shape is retained by
manifest v14. Table constraints accept 64 entries per modeled kind and domains
accept 64 checks; the next item fails with SQLSTATE `54000` before catalog
mutation. Constraint positions are durable `pg_constraint` and referential
trigger OID identities with a 64-entry per-object stride. Raising the boundary
therefore requires an explicit format and OID migration that preserves existing
identities. Accepted-width catalog, WAL, checkpoint, empty-cache recovery, and
PostgreSQL differential coverage keep the current boundary exact. This remains
a documented PostgreSQL scale divergence rather than a compatibility claim.

Policy role lists now accept every configured catalog role plus `PUBLIC`.
Startup-sized committed, transactional, and recovery images preserve stored
role order through `pg_policy`, WAL, checkpoints, rollback, and object-cold
recovery; `pg_policies` sorts names like PostgreSQL's system view.
The WAL reader retains the former 64-role record kind while new records carry a
wide role count.
Enum label sets now use startup-sized committed, transaction-private, and
recovery images through `max_enum_labels_per_type` (default 256). Savepoint
chains retain exact prior images, PostgreSQL's pre-commit safety rule uses the
committed-member boundary without a 64-bit mask, and new WAL records carry a
32-bit label count while the former one-byte record remains readable.
Wide allocation-forbidden catalog checks also removed scalar subqueries' former
one-row temporary object publication; their cardinality-bounded result now
stays in the startup-sized external-sort chunk.

Path and polygon values now use exact statement-arena slices for parsing,
operators, construction, and binary receive. Index summaries and binary send
stream components without a per-value point array. Text and binary round trips,
wide operators, GiST lookup, checkpoint publication, and empty-cache object
recovery are qualified with 300-point values beyond the former 128-point and
2 KiB text limits; the SQL boundary is compared with PostgreSQL.

Full-text values now use statement-arena lists through parsing,
canonicalization, matching, ranking, headline generation, set-returning
functions, and binary receive. Binary send and index token extraction stream
the canonical value without rebuilding a bounded tree. PostgreSQL's 1 MiB
vector storage, 2,046-byte text lexeme, 2,047-byte binary lexeme, and
256-position-per-lexeme boundaries remain explicit; query wire nodes and child
offsets retain their 32-bit widths. Allocation-forbidden execution, binary
round trips, GIN and GiST indexes, checkpoint publication, empty-cache object
recovery, and a PostgreSQL 18.6 differential cover a 600-lexeme vector with
2,400 positions and a balanced 599-node query beyond the former 512-item and
2,048-total-position envelopes. The same vector is unnested after forced spill
and empty-cache recovery so the external lateral path preserves its record
shape and type identity.

Grouping now follows PostgreSQL 18's target-list width of 1,664 distinct
expressions, 4,096-set expanded-product limit, 12-element `CUBE` limit, and
31-argument `GROUPING()` result width. Variable-width arena bitmaps replace the
former 64-bit mask. Execution retains encoded results in the persistent arena
tail and recycles each set's scans and aggregate scratch, so the exact 4,096-set
boundary runs allocation-free in the default statement arena. The accepted and
next rejected widths, SQLSTATEs, messages, cross-word membership, and results
are qualified against PostgreSQL 18.6.

Query results now follow PostgreSQL 18's 1,664-column target-list boundary.
Simple, scoped, and set-operation execution, Statement Describe, per-column
text and binary Bind formats, and the exact 1,665-entry error are qualified
without runtime allocation against PostgreSQL 18.6. Query threads reserve one
128 MiB fixed stack and the default configuration reserves a 32 MiB statement
arena at startup for the statically bounded planning, execution, and protocol
scratch at the complete width.

Stored tables, views, named composites, and record column definition lists now
follow PostgreSQL 18's 1,600-column relation boundary. Table-function results
use the independent 1,664-column executable tuple boundary. Wide column sets
replace one-word masks in dependencies, triggers, publications, privileges,
indexes, checkpoints, and WAL; older durable records remain readable. Compact
64-bit scan proofs remain a physical optimization, while authorization retains
the exact SQL-visible columns when a high ordinal requires full-row decoding.
The accepted widths, high-column DML and metadata, exact 1,601/1,665 errors,
allocation-free execution, WAL replay, checkpoint publication, empty-cache
object recovery, and raw-wire PostgreSQL 18.6 differential behavior are
qualified. Materialized recursive relations retain their synthesized typed
definition across fixpoint iterations, so recursion depth no longer multiplies
the complete relation-width metadata in statement memory. Plain and aliased
100-row recursion are qualified in the default 32 MiB arena. Routine candidate
probes read the active transaction's small signature fields in place, and
ordinary projections bypass set-returning function materialization, so widened
metadata is not copied or cleared per row. The 1,100-call and 70,000-row scroll
cursor differential completes against PostgreSQL 18.6. Logical replication
decodes maximum-width relation and tuple frames as validated borrowed wire
views, keeping the decoded message size independent of the 1,600-column limit.
CI preserves the complete library, curated PostgreSQL differential, and
10,000-statement seeded fuzz suites in deterministic shards below the
15-minute worker ceiling; the seeded sequence runs in five slices, the growing
forced-spill corpus runs in six, and instrumented auxiliary phases have
independent workers.

Join range tables and accumulated `USING` merge state now use exact
statement-arena slices rather than a 64-relation executor envelope. Compact
`u64` access-path proofs remain an optimization for at most 64 sources; wider
joins retain identity order and full-row decoding, preserving exact execution.
Allocation-forbidden and PostgreSQL 18.6 differential coverage qualifies 128
relations through cross and `USING` joins, materialization, windows,
subqueries, plans, stored views, and joined DML. Explicit `USING` lists now
follow the 1,600-column relation shape.

Routine call signatures now match PostgreSQL's exact 100-input-argument limit.
The independent executable routine result boundary is 1,664 columns. Execution,
`pg_proc`, WAL, checkpoints, fixed-memory
operation, object-cold recovery, exact over-limit rejection, and PostgreSQL 18
differential behavior are qualified at the accepted boundaries.

Wire Parse/Bind and SQL `PREPARE`/`EXECUTE` now match PostgreSQL's unsigned
16-bit, 65,535-parameter count. Prepared metadata and portal values use their
existing startup byte reservations, while decoded values and inferred OIDs use
statement memory; configured byte or arena exhaustion is SQLSTATE `54000`.
The maximum count, explicit Parse OIDs, Statement Describe, per-parameter Bind
formats, execution, `pg_prepared_statements`, allocation-free serving, exact
over-limit SQL rejection, and PostgreSQL 18.6 differential behavior are
qualified.

JSON container, path, result, rendered-text, and JSON_TABLE row widths are
complete up to statement memory. XMLTABLE row width also follows statement
memory within the separately bounded XPath index. SQL array value width is
complete up to its durable 16-bit element count. Multirange component and
rendered-value widths now follow the value bytes and statement memory rather
than fixed 64-component and 1 KiB scratch arrays. Streaming readers cover
comparison, hashing, bounds, index summaries, and set-returning expansion;
canonicalization and set operations use exact arena slices. Allocation-free
128-component execution, binary Bind/result and COPY differential coverage
against PostgreSQL 18.6, indexes, WAL, checkpoints, and object-cold recovery
qualify the lifted boundary. Partition ancestry also requires every catalog
slot in the chain to remain transaction-visible, so dropped descendants cannot
reattach when a parent slot is reused. Statement lists other than the
exceptions above are bounded by statement memory.

For each changed capacity, qualify the full accepted width through parse or
wire input, execution, catalog output where applicable, journal encoding,
checkpoint retry, and object-cold recovery. Show exact startup-memory charging,
allocation-free execution, named exhaustion, and PostgreSQL differential
behavior at the boundary. A limit that remains by design must be documented at
the client-visible boundary and rejected before partial effects.

### Durable operations and availability

Health and readiness endpoints, Prometheus metrics, JSON capacity reporting,
text or JSON Lines logs, and the initial operations runbook use startup-bounded
memory. Durable readiness validates current writer-fence ownership through the
provider-neutral object client as well as observed publication health;
liveness remains a local event-loop probe.

Object-store credentials can come from a strict owner-only file. `SIGHUP` and
`pg_reload_conf()` parse a candidate into fixed buffers and conditionally renew
the writer fence before every root and block client adopts it. Failure retains
the installed credential, makes readiness false, and records a secret-free log
and metric. The external suite covers rejected and successful candidates,
durable work after old-credential revocation, and rotation back by signal.

Tagged releases build a locked Linux x86-64 executable, deterministic tarball,
and SHA-256 file. The archive contains starter configurations, hardened systemd
units, the failover monitor, license, and operator documentation. Pull-request
and tag CI extract the archive, execute the packaged binary, probe liveness,
exercise automatic promotion and writer fencing, and verify graceful shutdown.

A packaged monitor now observes one primary from one passive candidate,
requires consecutive bounded readiness failures plus final confirmation, and
runs one fixed promotion command. Candidate readiness proves durable ownership.
End-to-end qualification pauses a live primary, promotes from the same object
prefix, recovers durable data with empty local caches, resumes the displaced
process, and verifies readiness failure plus SQLSTATE `40001` on its connected
client. Cacheless object reads remain synchronous so a restartable statement
cannot repeatedly discard its only completed network response.

### Concurrent execution

Remove global query serialization with startup-bounded worker-private statement
state. Coordinate MVCC, locks, object I/O, cancellation, fairness, group commit,
and publication order through explicit backpressure. Demonstrate useful
one-through-N core scaling for read-only, write-heavy, and mixed workloads
without post-startup allocation or weaker durability.

The global execution arena and mutable DML row-selection scratch are now
startup-bounded sets selected together through exclusive dispatcher leases.
`query_workspace_slots` charges every `work_arena_bytes` and `table_rows`
reservation in the memory plan, rejects zero or more slots than connection
capacity, and exposes configured, active, and waiting counts through metrics
and capacity JSON. Lease ownership and its FIFO wait roster are themselves
charged exactly at startup; disconnect and failed-interest paths hand a slot to
the oldest live waiter without aliasing its arena or DML scratch. Backend
identity now belongs to the leased workspace and is republished through fixed
thread-local execution context when the workspace is selected. Storage no
longer carries a shared mutable connection selector, so temporary schemas,
advisory locks, backend statistics, signals, and LISTEN state cannot inherit
another worker's backend identity. Database identity now follows the same
workspace and thread-local boundary. Storage and WAL no longer carry mutable
database selectors, and selecting a leased workspace republishes its database
before catalog access or transaction WAL staging. The arena and DML workspace
sets no longer carry a shared active index: both resolve the typed worker-local
lease identity, so concurrent workers cannot redirect each other's scratch.
Streamed COPY transition rows are connection private, and logical subscription
bootstrap workers own the same fixed state independently. Interleaved client
streams therefore cannot clear or mix the row set observed by statement-level
transition triggers; every buffer is charged from `txn_rows` at startup.
Logical subscription apply and bootstrap COPY execution use the worker's own
arena and DML scratch rather than a client workspace. A readable dispatch or
parked retry now produces one typed completion carrying its exact workspace
lease and backend and database identity. The reactor validates and releases the
lease when engine work completes, retains only the session identity through the
shared publication barrier, and restores it before response cleanup. Connection
release and cross-session cancellation also restore the target identity at
their choke points. Readable work and parked retries now enter one allocation-
free FIFO whose capacity is exactly `query_workspace_slots`. Enqueue removes
socket read interest, records one slot-owned in-flight state, and retains the
exclusive workspace until the reactor drains the job into its typed completion.
This prevents separate read and write readiness events from racing one queued
dispatch. When one reactor turn has more ready connections than workspaces, it
drains queued scheduler chunks but retains every response for the turn's single
publication barrier. Queue capacity therefore does not reduce group commit
width. Engine execution remains reactor-serialized because catalog, cache, and
lock ownership is not thread safe. Parallel
dispatch must replace the local queue drain with fixed workers and make that
shared engine state safe while preserving transaction retry, object I/O
parking, group publication, and response barriers.

Engine ownership can now cross an operating-system thread boundary. Durable
and temporary block stores use shared mutex ownership between checkpoint and
spill paths, immutable external-run readers use a synchronized fixed pool, and
the deterministic object-store namespace uses a process-wide transferable
handle.
The owned POSIX locale records its single-owner transfer invariant explicitly.
A live-engine test moves ownership to another thread, executes SQL there, and
drops its storage and locale state on that thread. The engine remains one
owner at a time and reactor-serialized. Fixed workers still require shared
catalog, cache, and lock state to be partitioned
or synchronized before dispatch can overlap.

The effective SQL search path is now worker private. Every statement publishes
the path computed from its session settings into fixed thread-local execution
state, while view and schema-qualified nested execution swap and restore that
same worker's value. Storage no longer carries a shared mutable path that one
worker could change during another worker's name resolution. Cross-thread
isolation and live engine transfer retain direct regressions. Catalog, cache,
and lock ownership remain the shared mutation
boundaries before fixed workers can execute concurrently.

Command and durable commit snapshots are worker private as one fixed
thread-local visibility context. Statement entry publishes both values, while
data-modifying common table expressions and routine replay can lower only the
current worker's command snapshot. Storage row, SST, and durable index reads no
longer consult mutable engine-global snapshots. Cross-thread isolation retains
a direct regression alongside command-history, repeatable-read, and cold
recovery coverage. The remaining shared catalog and row mutation paths still
require synchronization before fixed workers can overlap execution.

Authorization graph traversal now has one startup-sized bitmap per query
workspace. Every role membership, object privilege, grant-option, and column
privilege check selects scratch through the typed worker-local lease identity,
so overlapping workers cannot collide on one mutable borrow or overwrite each
other's traversal. `query_workspace_slots` charges the bitmap and its container
exactly in the fixed memory plan. Shared role and ACL catalog mutation remains
serialized with the other catalog write paths.

Foreign statement context now follows the same workspace boundary. Each leased
workspace owns the transaction identity, isolation flag, and fixed savepoint
roster used while opening or resuming a remote transaction, with exact startup
charging through `query_workspace_slots`. A worker can no longer overwrite the
context consumed by another worker's foreign scan. Foreign transports now form
an exactly charged `max_foreign_sessions` pool keyed by local transaction and
remote endpoint. Each slot reserves its complete wire buffers at startup;
opening a second transaction or endpoint uses a distinct slot, while exhaustion
returns SQLSTATE `53300`. Savepoint, rollback, and commit commands visit every
remote session owned by the local transaction. Metrics and capacity JSON report
the configured and occupied slots. Each complete client and its ownership
record share a per-slot mutex, while assignment and release share one pool lock
so concurrent reservations cannot duplicate an endpoint or claim the same
capacity. A typed client guard retains the slot lock through remote activation.
Operations on one session serialize while distinct slots can drive their
sockets concurrently.

Cumulative relation, index, database, and function statistics now share one
fixed state protected by a mutex. Transaction nesting and pending counters use
the same boundary, so commit and rollback publish their relation and database
totals atomically while concurrent scans and function calls cannot collide on
interior borrows or lose updates. Function timing remains worker-local. The
statistics vectors retain their startup capacities and named exhaustion.

### Performance qualification

The [256-row](benchmarks/baselines/2026-09-20-postgresql18-local-apfs/README.md)
and [1,000-row](benchmarks/baselines/2026-09-20-postgresql18-local-apfs-1000/README.md)
exploratory baselines are complete against unmodified PostgreSQL 18 on its
normal local-storage durability path. Matched comparison workloads used the
same SQL and load. pos3ql published to an instrumented object-store fixture
backed by local temporary storage. Completed object reads formerly stranded
fixed slots and parked SP-GiST probes; mixed resident/spilled scans then
point-read immutable blocks. Both paths now advance at 1,000 rows. The larger
run used a 1 GiB fixed disk cache and a 120-second query timeout, unlike the
256-row run's 128 MiB cache and 30-second timeout. The same-process point
workload still made object requests, and explicit checkpoint pressure cut
throughput sharply. These short, shared-host runs cannot establish production
ratios or isolate dataset size from cache changes. Measure explicit warm-RAM,
warm-disk, and empty-cache states, then extend to larger datasets and logical
replicas before using the comparison to rank concurrency or storage changes.

Profile explicit checkpoint work by value-index rebuild, SST publication,
and garbage deletion. The 1,000-row maintenance case completed with 5,902
object requests and 4.36-second p99 query latency. Value-index rebuild now
uses the merged spill cursor instead of point-reading each spilled row; a
zero-cache regression reduced second-checkpoint object GETs from 837 to 84
while preserving indexed results after object-cold recovery. Measure the
remaining publication, garbage-deletion, and foreground interference costs
before ranking further changes. A focused
[1,000-row run](benchmarks/baselines/2026-09-20-checkpoint-value-index-1000/README.md)
records the revised path but is not a controlled timing comparison with the
earlier full suite. Preserve durability and fixed-memory behavior.

Checkpoint index writers now compare content identities against their last
published generation and reuse blocks already durable in that generation.
A zero-cache update-and-recovery regression reduced second-checkpoint block
PUTs from 30 on merged main to 19, including the unchanged GIN generation
blocks. Continue to attribute the remaining sort-run writes, SST publication,
garbage deletion, and foreground latency before changing their pacing. The
[clean 1,000-row focused run](benchmarks/baselines/2026-09-20-checkpoint-block-reuse-1000/README.md)
recorded 1,556 object PUTs versus 2,595 before reuse; its latency and cleanup
counts are exploratory on a shared host.

Checkpoint value-index sorting now consumes complete runs directly from the
sorter's startup buffer and spills only when that buffer fills. The zero-cache
one-row-update regression reduced second-checkpoint block PUTs from 19 to 11
without changing recovered indexed results. The
[clean focused run](benchmarks/baselines/2026-09-20-checkpoint-sort-runs-1000/README.md)
recorded 814 object PUTs and 553 DELETEs, versus 1,556 and 1,071 before this
change. The shared-host latency pair remains exploratory. Attribute the
remaining SST publication and garbage-deletion costs, then qualify foreground
interference at larger scale before changing checkpoint pacing.

The focused harness now runs matching mixed workloads with and without three
explicit checkpoints against actual PostgreSQL 18 as well as pos3ql. It
rejects missing checkpoint operations and records PostgreSQL's durability and
local storage settings. The [paired 1,000-row run](benchmarks/baselines/2026-09-21-postgresql18-checkpoint-1000/README.md)
completed without errors: pos3ql p99 was 72.39 ms without explicit
checkpoints and 1,447.94 ms with them; PostgreSQL p99 was 2.76 and 1.51 ms
in its much shorter samples. These shared-host timings are exploratory, and
PostgreSQL's local persistence has no equivalent object-request metric.
The [fixed-memory phase profile](benchmarks/baselines/2026-09-21-checkpoint-phase-1000/README.md)
attributes publication and cleanup in a four-second, 1,000-row run. Its
full request window reconciles all 846 object DELETEs: 550 from commit-batch
pruning and 296 from block garbage collection. The measured phase spans were
1.59 seconds for commit pruning, 1.52 seconds for value-index rebuild, 0.82
seconds for block deletion, and 0.54 seconds for row SST publication. The
profile includes cleanup after timed queries end, so these spans do not by
themselves identify foreground stall time.
The [foreground-correlation run](benchmarks/baselines/2026-09-21-checkpoint-stall-correlation-1000/README.md)
now aligns fixed-memory phase events with each client operation after the
worker barrier. Value-index publication intersected 17.65 seconds of summed
concurrent client latency, row SST publication 13.23 seconds, block deletion
6.74 seconds, and commit pruning 5.79 seconds; local cleanup intersected only
3 milliseconds. The profile covers automatic work and the three explicit
commands, and one operation may span several phases, so these associations do
not form an exclusive causal decomposition. Next, reduce publication writes
and bound cleanup work per dispatch beat while preserving durability, fixed
memory, and provider neutrality.
The [final-slice publication run](benchmarks/baselines/2026-09-21-checkpoint-final-slice-1000/README.md)
removes one source of repeated publication. A sweep now publishes in the beat
that writes its final stale table slice when no merge beat is due. It
still yields when another table is stale or a bounded merge beat is due. The
benchmark now completes an unmeasured settling checkpoint after each engine's
baseline, preventing earlier automatic work from entering the interference
window. In the clean settled run, three explicit checkpoints produced three
row SST and value-index generations, while shutdown added one metadata-only
manifest; the window recorded 765 object PUTs. The earlier un-settled profile
exposed redundant generations but is not a controlled timing comparison with
this corrected boundary.
The same CI run closed a transaction retry defect exposed by this scheduling
change: a cold row that parks final WAL staging now preserves the transaction
and its locks for retry, and a mark from any cleared transaction is no longer
treated as live undo state merely because cleanup retained the same numeric
transaction identity.

Checkpoint commit pruning now joins legacy SST and block garbage collection in
the paced post-publication maintenance state machine. One dispatch beat deletes
at most `checkpoint_delete_objects_per_beat` objects from one namespace,
counting commit batches and descriptors separately; its default is 16. The
larger fixed garbage staging batch remains 4,096 so paced beats do not repeat a
complete namespace scan. Explicit `CHECKPOINT` still drains all batches before
returning.
The [clean pacing run](benchmarks/baselines/2026-09-21-checkpoint-deletion-pacing-1000/README.md)
limited every profiled commit and block deletion event to 16 objects. Maximum
event spans were 54 ms for commit pruning and 52 ms for block deletion, versus
665 ms and 340 ms when the prior run placed as many as 244 and 131 deletes in
one event. The run made exactly four namespace scans per publication even
though block deletion took 33 beats, matching the prior unpaced run's four
scans per publication. Total work and foreground samples differ, and explicit
checkpoints execute their batches contiguously, so this establishes the
per-beat and scan boundaries rather than an end-to-end latency ratio. Actual
PostgreSQL 18.6 remains the reference for the paired SQL workload on its
documented local durable tier.
Value-index publication now follows physical column dependencies. Each binding
records the columns used by its key, partial predicate, and included payload;
the precommit row path compares encoded column payloads without allocation and
dirties only intersecting bindings. Insert, delete, replay, rewrite, and index
maintenance retain conservative invalidation. A recovery regression covers
primary, covering, expression, partial, and GIN indexes, including a WAL-only
insert checkpointed after an object-cold restart.
The [selective publication run](benchmarks/baselines/2026-09-21-checkpoint-value-dependencies-1000/README.md)
had the same settled three value-index events as the final-slice run. Their
block PUTs fell from 224 to 24 and measured phase time from 1.03 seconds to
0.19 seconds; full-window PUTs fell from 765 to 576. Actual PostgreSQL 18.6
completed the matched SQL and checkpoint workload on its recorded local
durable tier. These shared-host measurements are exploratory and do not equate
PostgreSQL storage with pos3ql object traffic.
Checkpoint row reslicing now retains a compatible earlier slice and appends
only versions committed after its captured LSN. Relation replacements rebuild
without a reusable published base. Fixed startup metadata
preserves warm reads after publication and maps rows to their containing SST
when later memory pressure evicts them. Filled generation rosters retain their
slice and LSN boundary through an object-store failure.
A deterministic regression reduced a one-row reslice from the first slice's 13
block PUTs to 5; focused fault injection and the storage VOPR corpus qualify
retry, deferred eviction, and object-cold recovery. The
[incremental-reslice run](benchmarks/baselines/2026-09-21-checkpoint-row-reslice-1000/README.md)
exercised 35-then-5 and 137-then-5 block sweeps plus the required full-roster
rebuild path. Its matched actual PostgreSQL 18.6 workload completed with
durability enabled on the recorded local tier. The run contained substantially
more automatic checkpoint work than the prior profile, so its timing and total
traffic are not a controlled comparison.
The [10,000-row scale run](benchmarks/baselines/2026-09-21-checkpoint-reslice-scale-10000/README.md)
found and closed repeated staged value-index rebuilds plus row-by-row cold
`ANALYZE` and `CREATE INDEX` reads. In the completed profile, affected value
indexes dominated compatible reslices in aggregate: 14 events took 41.03
seconds and wrote 1,017 blocks, while 13 row deltas took 0.92 seconds and wrote
197. Most value-index events read no durable blocks, confirming that unchanged
staged bindings were retained. One full-roster row rewrite remained the largest
individual event at 27.50 seconds, 6,600 block reads, and 1,337 writes; it drove
the 30.27-second maximum foreground latency.
The [bounded full-roster rerun](benchmarks/baselines/2026-09-21-checkpoint-full-roster-bounded-10000/README.md)
replaces that rewrite with restartable pair-merge beats. One fixed completed
merge slot per table lets several filled rosters prepare for one manifest
publish. A deterministic two-table regression interleaves a foreground update,
bounds each dispatch, injects an object-store failure, and verifies warm and
object-cold results. The exact profile contained no `row_sst_full` event: 64
schedule beats read 4,712 blocks over 16.10 seconds, and 207 write beats wrote
844 blocks over 4.24 seconds. Their largest events were 447.94 ms and 61.66 ms,
versus the former 27.50-second dispatch. Maximum foreground latency fell from
30.27 to 6.24 seconds, while p99 fell from 5.60 to 4.03 seconds. The workload's
other event counts changed, so these shared-host results remain diagnostic
rather than a controlled production ratio. Actual PostgreSQL 18.6 completed
the matched local-durable workload at 3.90 ms p99 and 79.31 ms maximum latency.
The [paced value-index rerun](benchmarks/baselines/2026-09-22-checkpoint-value-index-pacing-10000/README.md)
retains one fixed-memory sorted source per affected binding and streams its
immutable output through restartable beats. Its 74 writer beats took 153.38 ms
and wrote 28 blocks; the largest took 15.57 ms and four PUTs, with no four-PUT
or eight-GET bound violation. A deterministic regression also invalidates an
in-progress source after an indexed commit, retries a failed remote write, and
verifies warm and object-cold results. The same profile makes the next boundary
explicit: 24 value-index schedule events spent 21.26 seconds collecting and
externally sorting entries, and the largest occupied 1.41 seconds while writing
22 temporary run blocks. Pace value-index source collection and external run
generation without rescanning rows or weakening fixed-memory sorting, dirty-LSN
invalidation, content reuse, or provider neutrality, then repeat the exact
10,000-row profile before changing the durable row representation. The run's
row-merge counts and end-to-end latency differed substantially from its
predecessor, so the shared-host aggregate timings are diagnostic rather than a
controlled pacing ratio.
The [paced source-and-sort rerun](benchmarks/baselines/2026-09-22-checkpoint-value-index-schedule-10000/README.md)
completes that boundary. Source collection retains resident and merged-spill
cursors across beats, initializes long spill-generation lists incrementally,
reloads only displaced member buffers, and decodes only each binding's physical
PAX dependencies. Startup-allocated binary carry state writes and merges
provider-neutral temporary SSTs without rescanning source rows. The exact
profile recorded 461 schedule events over 6.70 seconds; the largest took 36.42
ms, and none exceeded eight GETs or four PUTs. The preceding profile's largest
schedule event took 1.41 seconds with 100 GETs and 22 PUTs. Direct regressions
also cover a deferred source row across a carry merge, cursor resumption at a
data-block boundary, dirty-generation restart, object-store retry, publication,
and object-cold recovery. Storage VOPR fault qualification additionally proved
that pending versions may appear between source beats without changing the
committed generation; checkpoint collection now keeps its resident/spill seam
stable by classifying only the committed home and matching the spill commit
LSN.

The durable format contract now covers manifest v13-to-v14 upgrade and
v2/v3/v4 mixed row generations. New v4 full slices retain PAX, while v4 deltas
pack compressed canonical row groups into verified containers. Row merge
scheduling reads keys and tombstones from PAX descriptors without fetching
column extents, and both schedule and write beats have provider-neutral object
read boundaries. A deterministic 128-row delta writes four objects and survives
empty-cache recovery.
The [clean repeated 10,000-row profile](benchmarks/baselines/2026-09-22-checkpoint-row-format-10000/README.md)
reduced the schedule maximum from 121 GETs and 401.72 ms to eight GETs and
23.91 ms. Its two deltas each wrote four blocks, down from 27, and their maximum
fell from 127.91 to 25.62 ms. Event counts and shared-host timing differ, so the
request bounds are the controlled result.

The following [row-merge profile](benchmarks/baselines/2026-09-22-checkpoint-row-merge-containers-10000/README.md)
changed the diagnosis of the remaining read amplification. Retaining decoded
source groups across write beats removed repeated setup, but a cursor-only run
showed that full PAX row reconstruction still issued a ranged GET for every
column. Full-row decoding now fetches each shared packed container once while
selective readers retain column pruning. Source cursors use sparse-key seeks
when the merge schedule skips blocks, so their retained state cannot turn a
pruned range into a linear scan. The clean profile reduced `row_merge_write`
from 5,782 to zero GETs, 440 to 228 beats, and 21.31 to 7.08 seconds of summed
phase time; the largest event fell from 1.21 seconds to 334.94 ms. A separate
cache-disabled regression exercises the provider path through warm and cold
recovery with 19 total GETs and at most six in one beat. Fixed startup memory,
snapshot pruning, retry, and v2/v3/v4 format identity remain explicit.

The [immutable-group reuse profile](benchmarks/baselines/2026-09-22-checkpoint-row-merge-reuse-10000/README.md)
closes the row-merge output boundary. A complete PAX group whose physical
versions all survive schedule pruning now enters the merged generation by its
verified immutable reference. The new roster names the descriptor and every
column container, while changed, snapshot-pruned, duplicate, and removable
tombstone groups use the ordinary writer. Canonical checksums, mixed v2/v3/v4
reads, fixed memory, retry idempotence, paced traffic, and empty-cache recovery
remain covered. Across 234 write beats, PUTs fell from 922 to 90, 212 beats
wrote no object, and summed phase time fell from 7.08 to 1.07 seconds. The
largest event fell from 334.94 to 39.24 ms. The run completed the same
configured workload and actual PostgreSQL 18 comparison, though its duration
floor admitted more foreground operations, so aggregate timing is exploratory.

The immutable-group reuse profile left `value_index_schedule` as the largest
checkpoint construction boundary: 398 events made 910 GETs and 162 PUTs. The
next change therefore targeted its physical source and external-run work while
retaining the startup-sized binary carry, exact key ordering, selective PAX
dependency reads, retry restart, four-PUT and eight-GET beat limits, and
publication identity.

The [incremental value-index profile](benchmarks/baselines/2026-09-23-checkpoint-value-index-delta-10000/README.md)
completes that boundary. Ordinary committed changes now sort only resident rows
newer than the published index LSN, then merge them with a restartable ordered
stream of the immutable generation. A startup-bounded set captures the exact
row identities represented by that delta and remains stable through output and
retry. Suppressing those rows from the base makes key moves, predicate exits,
deletes, covering payload changes, and posting changes exact replacements
rather than an accumulating overlay. Object-resident rows whose binding stayed
clean retain their published entry. Relation rewrites, catalog changes, and
`REINDEX` retain the complete source walk. Fixed startup buffers cover both
ordinary roster chains and navigation trees, and retries restart both inputs.

Across a similar two-generation value-index workload, schedule plus write work
fell from 460 events, 910 GETs, 204 PUTs, and 4.05 seconds to 76 events, zero
object GETs, 62 PUTs, and 0.26 seconds. Schedule alone became 12 CPU-only events
totaling 2.06 ms. Every value-index beat stayed within four PUTs and eight GETs.
The run completed 1,372 foreground operations versus 825 in the preceding
duration-floor run, so aggregate traffic and latency are diagnostic rather than
a controlled ratio. Actual PostgreSQL 18.6 completed the matched SQL and
checkpoint workload on its recorded local
durable tier at 2,786 operations per second and 9.86 ms p99; its persistence
path has no pos3ql object-request equivalent.

Every paced value-index and row-merge event stayed within its object-I/O beat
limits. Correctness validation rejected a resident-only row-SST delta shortcut
because a changed row may spill before checkpoint. The restored complete
logical scan made 228 GETs over two delta events, with one 702.96 ms event. This
was the next measured construction target. Row merge made 47 GETs over 42
schedule beats and 100 PUTs over 235 write beats; their totals were 0.14 and
1.02 seconds, with largest events of 25.38 and 24.18 ms. Commit pruning was a
separate 1.42-second post-publication cost.

The complete storage VOPR range then exposed two interleavings hidden by the
profile: consulting a live committed LSN during output could suppress an
unchanged base entry after a later non-indexed commit, and rollback could leave
a redundant spilled overlay state that shadowed an immutable index candidate.
The captured row-identity set now supplies the stable merge boundary, while
rollback removes the redundant overlay state. Deterministic seeds 460259
through 460274 cover outage, cold-start, warm-restart, and rollback variants.

The [exact row-delta profile](benchmarks/baselines/2026-09-23-checkpoint-row-delta-discovery-10000/README.md)
closes the remaining table-wide delta scan. Each committed row retains an
unpublished-change LSN in the startup-sized overlay until the matching table
generation publishes. Delta discovery walks only those resident identities;
the row bytes may be in the heap or an immutable generation. Full rewrites
retain the complete logical walk, compatible reslices retain their captured
LSN boundary, and ordinary deltas preserve the resident historical images
required by pinned-snapshot pair merges. Recovery rejects overlay exhaustion
on a replayed delete instead of silently losing its tombstone.

Across the same two row-delta events, object GETs fell from 228 to zero,
summed phase time from 734.49 to 33.34 ms, and the largest event from 702.96
to 19.84 ms. The eight PUTs and four-PUT event maximum were unchanged. A
cache-disabled regression proves zero-GET discovery after evicting every
redundant base row, then verifies the initial delta and an in-progress reslice
after object-cold recovery. A 24-generation pinned snapshot and the complete
16-seed storage VOPR range cover merge pruning, retry, outage, corruption, and
restart interleavings.

The run's row-merge schedule made 216 GETs over 62 events, versus 47 over 42
events in the preceding duration-floor run, while retaining the eight-GET
event maximum. Row-merge output made 74 PUTs over 233 events and retained its
five-PUT maximum. This differing generation shape means shared-host aggregate
traffic and timing cannot rank another implementation change. The next
performance evidence should use pinned representative hardware and an
independently operated compatible object store, then compare like generation
shapes before changing the already bounded merge and cleanup beats. Actual
PostgreSQL 18.6 completed the matched SQL and checkpoint workload on its
recorded durable local tier; its persistence path has no pos3ql object-request
equivalent.

The benchmark harness now runs the same pos3ql workload against its
instrumented fixture, pinned MinIO, and pinned SeaweedFS, pairing every backend
run with contemporaneous host-available and resource-matched vanilla
PostgreSQL 18 controls. The matched container receives the CPU count available
to pos3ql and pos3ql's exact fixed startup memory plan, with swap borrowing
disabled; the report rejects missing or unequal limits. Environment
artifacts identify the implementation, immutable container reference, backing
storage, artificial latency, and request-metric availability. A combined
report rejects mismatched commits, binaries, and workload shapes. Exact
provider request attribution remains available from the fixture; MinIO and
SeaweedFS timings report those counters as unavailable. Local containers expand
implementation coverage but do not satisfy the remaining independently
operated object-store qualification.

The [clean 10,000-row matrix](benchmarks/baselines/2026-09-23-object-store-matrix-10000/README.md)
completed every fixture, MinIO, SeaweedFS, and vanilla PostgreSQL 18.6 workload
without error. pos3ql mixed-baseline throughput was 206.83, 157.15, and 154.35
operations per second with p99 latency of 76.77, 103.34, and 114.11 ms;
checkpoint-overlap throughput was 100.14, 75.21, and 46.94 operations per
second with p99 latency of 758.77, 1,948.90, and 2,087.74 ms. The paired
PostgreSQL checkpoint runs recorded 2,145.72, 2,935.24, and 1,629.75 operations
per second with 10.96, 6.82, and 14.94 ms p99. The sequential PostgreSQL
variation, differing completed foreground counts, and row-merge output of 58,
497, and 494 block PUTs show that this shared-host duration-floor sample cannot
rank providers. It establishes executable coverage and exposes the checkpoint
cost on each stated local setup; the representative run remains outstanding.

The [resource-matched 10,000-row matrix](benchmarks/baselines/2026-09-23-resource-matched-postgresql-10000/README.md)
adds a second stock PostgreSQL 18.6 control for every backend. Docker recorded
a 12-CPU quota matching the CPU count available to pos3ql and a
995,951,270-byte limit matching its fixed memory budget, with swap borrowing
disabled, while retaining the preceding
host-available control. All 27 engine and scenario combinations completed
without error. The matched PostgreSQL mixed-baseline samples measured 6,442.94,
1,939.32, and 3,660.41 operations per second for fixture, MinIO, and SeaweedFS;
pos3ql measured 202.10, 215.30, and 276.55. Sequential shared-host variation,
different completed operation counts, and the distinct durable tiers still
prevent a controlled production ratio. The resource constraint and provenance
are now explicit; the representative independently operated run remains
outstanding.

The benchmark harness now has a strict `external` object-store profile for
that remaining run. It requires TLS and an explicit assertion that the service
is independently operated, uses a fresh recorded object prefix, supports
temporary session credentials and path or virtual-hosted addressing, and keeps
all credentials out of retained artifacts. Retained evidence records the
service identity, endpoint, bucket, region, backing storage, network path,
pinned host, cache storage, and both PostgreSQL storage descriptions. Derived
reports distinguish this topology from exploratory same-host services. The
long-running measurement and publication below still require representative
infrastructure and remain outstanding.

The full performance suite now covers the previously listed large-catalog
shape with 128 configurable ordinary views in an isolated schema. It measures
exact relation-name resolution and `pg_class` lookup in warm memory and after
empty-local-cache recovery; both vanilla PostgreSQL controls run the same
catalog setup and warm workload. Smoke mode exercises 16 relations, raw
artifacts record the selected count, and matrix identity checks reject unequal
catalog sizes. The focused checkpoint suite keeps this count at zero so the
new setup does not alter its publication profile.

Repeat long-running measurements on pinned representative hardware with an
independently operated compatible object store. Record PostgreSQL's local
storage medium and durability settings and pos3ql's object store, network,
and cache conditions. Compare end-to-end latency and throughput for the stated
setups while reporting each system's distinct persistence costs separately.
Do not model PostgreSQL as an object-storage database or treat its cache and
object-request metrics as equivalent to pos3ql's. Publish schema-versioned raw
artifacts and a reproducible report with throughput, latency percentiles, CPU,
fixed-memory occupancy, object requests and bytes, recovery time, checkpoint
interference, replica freshness, and physical access-path counters.

Cover warm RAM, warm disk, empty local caches, concurrent writes, checkpoint and
compaction pressure, parameterized joins, large catalogs, and one-through-N
logical replicas. CI continues to gate correctness, memory bounds, request
shape, cold recovery, and access-path use. Absolute production performance
claims require the representative runs.

## Completion criteria

The production roadmap is complete when:

- every advertised SQL and wire shape has a documented PostgreSQL or explicit
  implementation boundary, and every accepted configuration survives
  checkpoint and object-cold recovery at its declared capacities without
  truncation or post-startup allocation;
- format migration, monitoring, credential rotation, packaging, and runbooks
  pass end-to-end operational tests;
- concurrent execution scales across the supported worker range while
  preserving MVCC, durability, fixed memory, and backpressure; and
- published representative benchmarks substantiate the latency, throughput,
  recovery, replica, memory, and object-request claims.
