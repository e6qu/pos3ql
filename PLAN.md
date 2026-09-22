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

This is not yet a production-complete topology: one process serializes query
execution; writer fencing, promotion, backup and point-in-time recovery,
durable-format migration rules, operational interfaces, and representative
long-run performance evidence remain open.

## Remaining production work

### Compatibility and capacity

Maintain a capacity inventory that distinguishes PostgreSQL protocol or type
bounds, durable-format bounds, configurable startup capacities, statement-memory
bounds, and narrower implementation limits. For each narrower limit, record the
accepted shape, SQL or wire error, storage representation, and the reason for
keeping or lifting it. Explicit rejection protects correctness but does not
make a smaller accepted surface PostgreSQL-compatible at that width.

The known narrower limits to resolve or justify are:

- 64 Bind and SQL `PREPARE` parameters against the PostgreSQL wire count of
  65,535; 64 `GROUP BY` terms and 256 grouping sets;
- 128 result columns; 64 joined relations and 64 `USING` columns;
- durable 64-item definition shapes, including constraints, routine arguments,
  enum labels, and policy roles; and
- per-value `tsvector`/`tsquery`, multirange, and geometry widths.

JSON container, path, result, rendered-text, and JSON_TABLE row widths are
complete up to statement memory. XMLTABLE row width also follows statement
memory within the separately bounded XPath index. SQL array value width is
complete up to its durable 16-bit element count. Statement lists other than
the exceptions above are bounded by statement memory.

For each changed capacity, qualify the full accepted width through parse or
wire input, execution, catalog output where applicable, journal encoding,
checkpoint retry, and object-cold recovery. Show exact startup-memory charging,
allocation-free execution, named exhaustion, and PostgreSQL differential
behavior at the boundary. A limit that remains by design must be documented at
the client-visible boundary and rejected before partial effects.

### Durable operations and availability

- Define durable-format versions, compatibility rules, and online or offline
  migration procedures before changing persisted representations.
- Implement backup, restore, and point-in-time recovery; test restore into empty
  local caches across checkpoints and retained commit history.
- Add single-writer ownership and fencing before promotion or failover. Prove
  that an old writer cannot publish after ownership changes, including delayed
  object requests and restart races. Multiple writable processes on one prefix
  remain unsupported until this gate passes.
- Provide health and readiness reporting, metrics, structured logs, capacity
  reporting, secure credential rotation, packaging, and operational runbooks.
  Exercise operator recovery paths end to end.

### Concurrent execution

Remove global query serialization with startup-bounded worker-private statement
state. Coordinate MVCC, locks, object I/O, cancellation, fairness, group commit,
and publication order through explicit backpressure. Demonstrate useful
one-through-N core scaling for read-only, write-heavy, and mixed workloads
without post-startup allocation or weaker durability.

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

`value_index_schedule` is now the largest checkpoint construction boundary. It
made 910 GETs and 162 PUTs over 398 events in the reuse profile. Reduce its
physical source and external-run work while retaining the startup-sized binary
carry, exact key ordering, selective PAX dependency reads, retry restart,
four-PUT and eight-GET beat limits, and publication identity. Compare the
result with actual PostgreSQL 18 under the same SQL workload while continuing
to report its local durable tier separately from pos3ql object traffic.

Repeat on larger datasets and representative hardware before asserting
production ratios.

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
- backup and point-in-time recovery, format migration, writer fencing,
  promotion, monitoring, credential rotation, packaging, and runbooks pass
  end-to-end operational tests;
- concurrent execution scales across the supported worker range while
  preserving MVCC, durability, fixed memory, and backpressure; and
- published representative benchmarks substantiate the latency, throughput,
  recovery, replica, memory, and object-request claims.
