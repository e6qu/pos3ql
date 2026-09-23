# Performance and scaling boundary

pos3ql has a reproducible measurement harness, not a blanket production
performance claim. The same dependency-free PostgreSQL v3 client drives
pos3ql and PostgreSQL 18; every run records database identity, workload shape,
latency and throughput summaries, and resource evidence as schema-versioned
JSON. `tools/benchmark-report.py` derives a report from those raw files.

## Current topology

- One server process owns one writable database state and
  serializes query execution. Startup-sized pools bound memory and make
  saturation an explicit error.
- Object storage is durable; memory and local disk are disposable caches.
  Immutable journal batches and a compare-and-swap commit head are published
  before success reaches a client.
- One reactor turn is the group-commit unit. Readable clients and statements
  resumed after row-lock or object-read waits retain their response in their
  fixed connection buffer, share one journal publication barrier, and only
  then flush. A failed barrier replaces every guarded success response with an
  explicit unknown-outcome error. No runtime queue or buffer grows.
- Logical publications and subscriptions build independently durable read
  copies. They are asynchronous logical replicas, not transparent
  shared-storage replicas.
- Several writable processes on one object prefix remain unsupported. Writer
  fencing, ownership leases, automatic failover, and a read-only
  shared-snapshot protocol do not exist.
- Eligible two-source equi-joins use a bounded hash build for physical tables,
  synthesized catalogs, and derived tables, including external runs. NULL keys,
  duplicate matches, residual ON predicates, and LEFT JOIN preservation share
  one execution path. The fixed build-entry ceiling still bounds eligibility;
  larger builds choose a nested-loop plan before execution.
- Schema-only catalog resolution reads shared definitions without constructing
  rows or recursively describing catalog-backed views. Resolved view OID
  lookups do not enumerate unrelated indexes. Reverse relation-OID lookups
  allocate only the rendered name, not a complete index catalog per cast.

## Running the suite

The smoke suite needs Rust, Python 3, and `nc`:

```sh
tools/run-performance.sh smoke /tmp/pos3ql-performance-smoke
```

The full and checkpoint suites need Docker for their resource-matched
PostgreSQL 18 control. `POS3QL_BENCH_POSTGRES_PORT` can name an existing
PostgreSQL 18 instance for the host-available control, but the matched control
still runs in Docker:

```sh
tools/run-performance.sh full ./performance-results/local
```

To repeat only setup and mixed checkpoint pressure without replicas, use
`checkpoint` mode. It runs the same workload against vanilla PostgreSQL 18,
retains the same raw workload schema, and requires
`POS3QL_BENCH_REPLICAS=0`:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_DISK_CACHE_MIB=1024 POS3QL_BENCH_TIMEOUT_SECONDS=120 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint
```

The default pos3ql durable tier is the instrumented fixture. Select a pinned
local MinIO or SeaweedFS container with `POS3QL_BENCH_OBJECT_STORE`:

```sh
POS3QL_BENCH_OBJECT_STORE=minio \
  tools/run-performance.sh checkpoint ./performance-results/minio
POS3QL_BENCH_OBJECT_STORE=seaweedfs \
  tools/run-performance.sh checkpoint ./performance-results/seaweedfs
```

Run the fixture, MinIO, and SeaweedFS consecutively, each paired with its own
host-available and resource-matched vanilla PostgreSQL 18 controls, with:

```sh
tools/run-performance-matrix.sh checkpoint ./performance-results/object-matrix
```

The combined report rejects different commits, binaries, or workload shapes
across the three runs. The fixture retains exact request and byte counters and
defaults to 2 ms injected request latency. MinIO and SeaweedFS run without
artificial latency and report request metrics as unavailable rather than
substituting an estimate. Their immutable image references can be overridden
with `POS3QL_BENCH_MINIO_IMAGE` and `POS3QL_BENCH_SEAWEEDFS_IMAGE`; each run
also records Docker's resolved image ID. These local containers are independent
S3-compatible implementations on the benchmark host, not independently
operated object-storage services.

Use the `external` profile for the representative run against an existing,
independently operated S3-compatible service. It requires TLS, an existing
bucket, explicit service and infrastructure descriptions, and credentials with
read, write, list, and delete access. The endpoint is a `host:port` authority,
not a URL. Path addressing is the default; set
`POS3QL_BENCH_OBJECT_STORE_ADDRESSING=virtual_hosted` when the service requires
virtual-hosted buckets. Temporary credentials can include
`POS3QL_BENCH_OBJECT_STORE_SESSION_TOKEN`, and a private trust root can be set
with `POS3QL_BENCH_OBJECT_STORE_TLS_CA_FILE`.

```sh
POS3QL_BENCH_OBJECT_STORE=external \
POS3QL_BENCH_OBJECT_STORE_ENDPOINT=objects.example:443 \
POS3QL_BENCH_OBJECT_STORE_BUCKET=pos3ql-benchmarks \
POS3QL_BENCH_OBJECT_STORE_REGION=region-1 \
POS3QL_BENCH_OBJECT_STORE_ACCESS_KEY="$ACCESS_KEY" \
POS3QL_BENCH_OBJECT_STORE_SECRET_KEY="$SECRET_KEY" \
POS3QL_BENCH_OBJECT_STORE_IMPLEMENTATION='service identity and available version' \
POS3QL_BENCH_OBJECT_STORE_BACKING='provider and durability description' \
POS3QL_BENCH_OBJECT_STORE_INDEPENDENTLY_OPERATED=1 \
POS3QL_BENCH_HARDWARE_DESCRIPTION='pinned host type and processor' \
POS3QL_BENCH_NETWORK_DESCRIPTION='location and path to object storage' \
POS3QL_BENCH_CACHE_STORAGE='local cache medium and filesystem' \
POS3QL_BENCH_POSTGRES_STORAGE='host-available PostgreSQL local medium' \
POS3QL_BENCH_MATCHED_POSTGRES_STORAGE='resource-matched PostgreSQL local medium' \
tools/run-performance.sh full ./performance-results/representative
```

The harness generates a unique object prefix for each external run and records
it with the endpoint, bucket, region, addressing mode, TLS state, network,
hardware, and cache descriptions. Set
`POS3QL_BENCH_OBJECT_STORE_PREFIX` only when a caller-managed unique namespace
is needed. External objects remain in that prefix after the run so cold
recovery evidence is reproducible; remove them after retaining the artifacts.
Access keys, secret keys, session tokens, and custom trust-root contents are
never written to benchmark artifacts; a custom trust root is identified only
by its SHA-256 digest. Provider request counters remain
unavailable unless the provider supplies separate evidence.

For an existing host-available PostgreSQL instance, set
`POS3QL_BENCH_POSTGRES_STORAGE` to describe its storage medium. The full run
requires PostgreSQL 18 with `fsync`, `full_page_writes`, and
`synchronous_commit` enabled. It records `version()`, durability and resource
settings, and, for Docker, the resolved image ID and mounts rather than
treating a mutable image tag as provenance. Its defaults
can be overridden with `POS3QL_BENCH_ROWS`,
`POS3QL_BENCH_TABLE_CAPACITY`, `POS3QL_BENCH_OPERATIONS`,
`POS3QL_BENCH_CLIENTS`, `POS3QL_BENCH_REPLICAS`, and, for the fixture only,
`POS3QL_BENCH_OBJECT_LATENCY_MS`. `POS3QL_BENCH_DISK_CACHE_MIB` sizes the
fixed local disk block cache (default 128 MiB), and
`POS3QL_BENCH_TIMEOUT_SECONDS` sets the per-query socket timeout (default 30
seconds). Both settings are recorded in the environment manifest; the timeout
also appears in each workload file. The capacity must cover both the setup
rows and all rows inserted by the configured clients and operations.
Full mode requires at least four clients so its synchronized update workload
can enforce the group-commit amplification bound; smaller values are rejected
before measurement.
Full mode also creates 128 ordinary views in an isolated schema and measures
exact relation-name resolution plus `pg_class` lookup as unrelated catalog
relations grow. Smoke mode uses 16 views. Override either count with
the positive `POS3QL_BENCH_CATALOG_RELATIONS` value; the focused checkpoint
mode requires zero so catalog setup cannot change its publication profile.
The selected count is recorded in the environment and every workload
artifact. Both PostgreSQL controls receive the same catalog and warm lookup
workload, while pos3ql also repeats it after empty-local-cache recovery.
CPU and resident-memory sampling uses `/proc` on Linux; peak RSS remains
available through `ps` on other supported systems.

The output directory contains an environment manifest with the commit, binary
hash, toolchain, machine, resources, and workload sizing; one raw JSON file
per workload; separate
recovery JSON intervals for initial/warm-disk/empty-local-cache starts, one
freshness interval per logical replica, Docker's resolved PostgreSQL image IDs
when Docker is used, `postgresql-server.json` and
`postgresql-matched-server.json` with PostgreSQL's settings, storage
description, and container limits,
the pos3ql startup log with its fixed memory plan, and a derived `report.md`.

Both controls run the stock PostgreSQL 18 image with its ordinary local
storage and database settings. The host-available control has no container CPU
or memory limit. The resource-matched control limits Docker to the CPUs
available to the pos3ql process and to pos3ql's exact fixed startup memory
plan; its memory and memory-plus-swap limits are equal so it cannot borrow swap.
The report validates those values from Docker metadata. Resource matching
equalizes availability, while each engine remains free to consume less than
its limit.

Both systems receive the same SQL workload, concurrency, row count, and
stopping rule; duration-bound runs can complete different operation counts.
pos3ql instead publishes durable state to object storage. A representative
comparison must record PostgreSQL's
storage medium and settings alongside pos3ql's object store, network, and cache
conditions. End-to-end latency and throughput can be compared directly for the
stated setups; storage request, cache-tier, and recovery measurements describe
each system's different persistence design and must be reported separately.
The bundled suite defaults to an instrumented object-store fixture backed by
local temporary storage. The object-store matrix adds pinned local MinIO and
SeaweedFS implementations. Their PostgreSQL comparisons are exploratory
baselines; the representative qualification in [PLAN.md](../PLAN.md) also
requires pinned hardware and an independently operated compatible object
store.

The first [10,000-row object-store matrix](../benchmarks/baselines/2026-09-23-object-store-matrix-10000/README.md)
preserves raw fixture, MinIO, SeaweedFS, and paired vanilla PostgreSQL 18.6
results. All workloads completed without error. Different duration-bound
foreground counts, row-generation shapes, and sequential PostgreSQL samples
make it implementation coverage and exploratory evidence rather than a
provider ranking.

The following [resource-matched 10,000-row matrix](../benchmarks/baselines/2026-09-23-resource-matched-postgresql-10000/README.md)
preserves both PostgreSQL controls for each backend. The matched containers
received a 12-CPU quota matching the CPU count available to pos3ql and its
exact 995,951,270-byte fixed memory plan, with an equal memory-plus-swap limit.
All 27 workloads completed
without error. The report keeps host-available and matched ratios separate and
retains the local-persistence, shared-host, and independently operated service
qualifications.

The complete [256-row](../benchmarks/baselines/2026-09-20-postgresql18-local-apfs/README.md)
and [1,000-row](../benchmarks/baselines/2026-09-20-postgresql18-local-apfs-1000/README.md)
exploratory baselines use actual PostgreSQL 18.6 on local APFS with durability
enabled, four clients, and no replicas. Raw JSON, PostgreSQL settings, startup
logs, and derived reports are preserved. The larger run uses a 1 GiB disk
cache and a 120-second query timeout; its 59 workloads all completed. A prior
1,000-row attempt timed out in SP-GiST text-prefix probing. Completed object
reads now release fixed-slot pressure, and sequential scans stream immutable
spilled rows alongside resident changes. The same-process point workload still
records object reads and does not qualify as fully resident RAM. Explicit
checkpoint pressure remains costly: the larger mixed run's p99 rose from 39.50
ms to 4,355.29 ms with three checkpoints.
The focused [checkpoint run](../benchmarks/baselines/2026-09-20-checkpoint-value-index-1000/README.md)
preserves raw results from the cursor change on a clean commit. Its different
suite order prevents a controlled timing comparison with that full baseline.
Profiling traced most checkpoint samples to value-index rebuilds reopening
spilled rows by point lookup. Checkpoint now derives each index entry from the
merged sequential spill cursor, including covering payloads and posting
tokens. Running the same zero-cache, 128-wide-row regression on merged main
`c7246853` and this branch counted 837 and 84 object GETs, respectively,
during its second checkpoint; both returned the same indexed rows after
empty-cache recovery. The regression source is
`checkpoint_value_indexes_stream_wide_spilled_rows_across_recovery` in
`src/sql/tests.rs`.
That fixture isolates repeated reads. It does not establish a representative
end-to-end speedup; publication and garbage collection remain in the measured
checkpoint path.
The next checkpoint change reuses unchanged index data, navigation, and roster
blocks whose identities appear in the published generation. In a zero-cache
1,000-row GIN fixture, the second checkpoint made 30 block PUTs on merged main
`c80eca72` and 19 after reuse; the updated row remained searchable after
object-cold recovery. This count includes sort-run and row-SST writes, so it
understates the fraction of index-writer requests avoided.
The [clean focused run](../benchmarks/baselines/2026-09-20-checkpoint-block-reuse-1000/README.md)
retains the raw 1,000-row comparison: object PUTs fell from 2,595 to 1,556
under the same workload settings. Single-run latency and cleanup counts remain
exploratory on the shared host.
The next [clean focused run](../benchmarks/baselines/2026-09-20-checkpoint-sort-runs-1000/README.md)
keeps a complete value-index sort in its startup buffer. A zero-cache
one-row-update regression counted 11 checkpoint block PUTs versus 19 before
this change, with the same indexed result after object-cold recovery. The
focused mixed run counted 814 object PUTs and 553 DELETEs versus 1,556 and
1,071 before it. Its 6.96-second elapsed time and 1,259.12 ms p99 are
exploratory single-run results; larger sorts still use external runs.
The [paired checkpoint run](../benchmarks/baselines/2026-09-21-postgresql18-checkpoint-1000/README.md)
now applies the same mixed SQL workload and three explicit checkpoints to
actual PostgreSQL 18 and pos3ql. Both completed 200 operations without errors.
The report records each engine's baseline and checkpoint p99, while keeping
PostgreSQL's local storage and pos3ql's object requests distinct. The 1,000-row
shared-host samples are too short to establish production ratios, particularly
for PostgreSQL's subsecond workloads.
The [checkpoint phase profile](../benchmarks/baselines/2026-09-21-checkpoint-phase-1000/README.md)
uses a compile-time diagnostic feature and a fixed stack log line for each
publication or cleanup phase. The focused four-second run attributed 550
object DELETEs to commit-batch pruning and 296 to block garbage collection,
matching all 846 DELETEs in the profile request window. Value-index rebuild
made 311 block PUTs and took 1.52 seconds; row SST publication made 117 and
took 0.54 seconds. The window includes cleanup after timed queries end, so
these phase totals do not measure query stall time or a production ratio.
The next [foreground-correlation run](../benchmarks/baselines/2026-09-21-checkpoint-stall-correlation-1000/README.md)
records every client operation and checkpoint phase on a common realtime axis,
while retaining monotonic durations within each process. It also fixes a
harness race that allowed all three checkpoint commands to finish before the
foreground barrier opened. In the clean four-second run, value-index rebuild
intersected 17.65 seconds of summed concurrent client latency, row SST
publication 13.23 seconds, block deletion 6.74 seconds, and commit pruning
5.79 seconds. Local post-publication cleanup intersected only 3 milliseconds.
These are associations across explicit and automatic checkpoint work, not an
exclusive causal decomposition or a production ratio.
The following [final-slice run](../benchmarks/baselines/2026-09-21-checkpoint-final-slice-1000/README.md)
closes the dispatch gap between writing the last stale table slice and
publishing its manifest when no merge beat is due. The harness also
completes an unmeasured checkpoint after each engine's baseline so earlier
automatic work cannot enter the interference profile. In the resulting
four-second window, three explicit checkpoints produced three row SST and
value-index generations; a fourth metadata-only manifest was published at
shutdown. The run recorded 765 object PUTs, 725 completed pos3ql operations,
and a 624.60 ms p99. PostgreSQL 18.6 completed 73,534 operations with a
0.59 ms p99 on isolated local APFS. The corrected window boundary prevents a
controlled timing comparison with the earlier correlation run, and these
shared-host measurements remain exploratory.
The [selective value-index run](../benchmarks/baselines/2026-09-21-checkpoint-value-dependencies-1000/README.md)
records dependency-aware publication from a clean commit. The precommit row
path compares encoded physical columns and invalidates only bindings whose key,
partial predicate, or included payload can change. Conservative invalidation
still covers membership changes, rewrites, maintenance, and WAL replay. With
the same settled shape of three row/value publication events as the final-slice
run, value-index block PUTs fell from 224 to 24 and their phase time from
1.03 seconds to 0.19 seconds; full-window PUTs fell from 765 to 576. The run
also preserves a matched workload against actual PostgreSQL 18.6 on its
recorded Docker-managed local durable tier. Timing remains exploratory on the
shared host, and PostgreSQL has no corresponding object-request measure.
The [incremental row-reslice run](../benchmarks/baselines/2026-09-21-checkpoint-row-reslice-1000/README.md)
retains compatible unpublished row generations when foreground commits make a
previous table slice stale. Later slices include only versions newer than the
captured LSN. Its deterministic regression wrote 13 blocks for the first
128-row slice and 5 for a one-row reslice, with deferred-eviction and
empty-cache recovery checks. The mixed profile exercised two 39-then-5 block
sweeps and a 137-block slice followed by four 5-block reslices. The run also
preserves the matched workload and durability settings from actual PostgreSQL
18.6. Its 12 row events and seven manifests differ from the prior settled run,
so aggregate latency and request totals are exploratory rather than a
controlled comparison.
The [10,000-row checkpoint scale run](../benchmarks/baselines/2026-09-21-checkpoint-reslice-scale-10000/README.md)
retains staged value-index generations whose dependencies did not change after
an earlier slice and streams object-resident rows through `ANALYZE`, `CREATE
INDEX` validation, and value-cache population. Full PAX scans verify one packed
container and its logical column frames per request group; selective execution
still uses ranged column reads. In the completed profile, 14 affected
value-index events wrote 1,017 blocks over 41.03 seconds, while 13 row deltas
wrote 197 over 0.92 seconds. One full-roster row rewrite read 6,600 blocks,
wrote 1,337, and took 27.50 seconds. It was the largest single phase and
coincided with the 30.27-second maximum foreground latency. This establishes
the next dispatch-bound target; it does not imply that PostgreSQL shares the
same persistence costs.
The [bounded full-roster rerun](../benchmarks/baselines/2026-09-21-checkpoint-full-roster-bounded-10000/README.md)
routes a dirty filled row-generation list through restartable pair-merge beats
before appending its delta. The exact profile contained no `row_sst_full`
event. Its 64 schedule beats read 4,712 blocks over 16.10 seconds, and 207
write beats wrote 844 blocks over 4.24 seconds; the largest events took 447.94
and 61.66 ms. Maximum foreground latency fell from 30.27 to 6.24 seconds and
p99 from 5.60 to 4.03 seconds. Other phase counts differed across the two
shared-host runs, so the change in aggregate timing is diagnostic rather than
a controlled production ratio. Actual PostgreSQL 18.6 completed the matched
local-durable workload at 3.90 ms p99 and 79.31 ms maximum latency. The largest
remaining checkpoint phase event was a 3.91-second value-index publication.
The [paced value-index rerun](../benchmarks/baselines/2026-09-22-checkpoint-value-index-pacing-10000/README.md)
separates source collection and sorting from immutable output writing. Its 74
writer beats totaled 153.38 ms and 28 block PUTs; the largest took 15.57 ms and
four PUTs, and no beat crossed its four-PUT or eight-GET boundary. The retained
fixed-memory source restarts after an object-store failure and is discarded
when a newer binding dirty LSN appears. The run's 24 schedule events remained
monolithic: they took 21.26 seconds, wrote 486 external-sort blocks, and reached
1.41 seconds and 22 PUTs in one dispatch. The run also performed more row-merge
work than its predecessor, so its 8.09-second p99 and 13.54-second maximum do
not isolate the effect of writer pacing. Actual PostgreSQL 18.6 completed the
matched local-durable workload at 10.32 ms p99 and 31.49 ms maximum latency.
The [paced source-and-sort rerun](../benchmarks/baselines/2026-09-22-checkpoint-value-index-schedule-10000/README.md)
then split source collection and fixed-memory external sorting across 461
beats. Its largest event made eight GETs, four PUTs, and took 36.42 ms, versus
100 GETs, 22 PUTs, and 1.41 seconds before pacing. The subsequent packed-v4
row format reduced each small row delta from 27 objects to four and bounded
row-merge scheduling at eight descriptor GETs per beat.
The [retained row-merge cursor profile](../benchmarks/baselines/2026-09-22-checkpoint-row-merge-containers-10000/README.md)
removed write-phase source rereads: `row_merge_write` fell from 5,782 to zero
GETs and from 21.31 to 7.08 seconds of summed phase time. The subsequent
[immutable-group reuse profile](../benchmarks/baselines/2026-09-22-checkpoint-row-merge-reuse-10000/README.md)
keeps complete pruned PAX groups by verified reference and names every shared
container in the new garbage roster. Across a similar 234 write beats, PUTs
fell from 922 to 90 and summed time to 1.07 seconds; 212 beats wrote no object.
Changed or pruned groups still use the ordinary writer. Cache-disabled
recovery tests read reused payloads after garbage collection. That profile left
value-index source and external-run work at 910 GETs and 162 PUTs.
The following [incremental value-index profile](../benchmarks/baselines/2026-09-23-checkpoint-value-index-delta-10000/README.md)
sorts only resident rows newer than each published value-index LSN and merges
that delta with a paced ordered stream of the immutable base. A startup-bounded
set captures changed row identities for stable suppression through output and
retry, while object-resident rows whose binding stayed clean retain their base
entry. Relation rewrites, catalog changes, and `REINDEX` still take the complete
rebuild path. Combined schedule and write work fell from 910 GETs, 204 PUTs,
and 4.05 seconds to zero object GETs, 62 PUTs, and 0.26 seconds. The schedule
was 12 CPU-only events totaling 2.06 ms. Per-beat limits, fixed memory, retry,
garbage collection, and object-cold recovery remain directly covered. Storage
VOPR seeds 460259 through 460274 also cover non-indexed commits during output
and rollback of object-resident changes. Correctness validation rejected a
resident-only row-SST delta scan because a changed row may spill before
checkpoint; its restored complete scan made 228 GETs and identifies the next
optimization boundary. The current run completed substantially more foreground
operations than its predecessor, so total traffic and latency are not a
controlled ratio. Actual PostgreSQL 18.6 remains a separate local-durable
reference with no corresponding object-request metric.

## Measured scenarios

| Scenario | Boundary measured |
|---|---|
| warm memory | Repeated hash-index point reads in the process that created and checkpointed the data |
| BRIN point pruning | Repeated warm and object-cold equality probes through a dedicated BRIN key and bitmap plan |
| BRIN inclusion filtering | Repeated warm and object-cold range-overlap probes through `range_inclusion_ops` and a bitmap plan |
| GiST inclusion filtering | Repeated warm and object-cold range-overlap probes through a dedicated GiST key generation |
| GiST geometric pruning | Repeated warm and object-cold point-in-box probes through immutable bounding-box navigation |
| GiST K-nearest-neighbor | Repeated warm and object-cold `<-> point` ordered limits through a covering geometric GiST generation |
| GIN array postings | Repeated warm and object-cold array-containment probes through exact-token posting navigation |
| GIN posting unions | Repeated warm and object-cold multi-token array-overlap probes, including an absent token |
| GIN full-text postings | Repeated warm and object-cold lexeme probes through exact-token posting navigation |
| GIN `jsonb_ops` postings | Repeated warm and object-cold top-level existence probes through exact-token posting navigation |
| GIN `jsonb_path_ops` postings | Repeated warm and object-cold containment probes through exact-token posting navigation |
| SP-GiST prefix filtering | Repeated warm and object-cold text-prefix probes through an SP-GiST plan |
| SP-GiST geometric pruning | Repeated warm and object-cold point-in-box probes through the same object-native navigation boundary |
| SP-GiST K-nearest-neighbor | Repeated warm and object-cold `<-> point` ordered limits through a covering k-d point generation |
| indexed tail range | Repeated selective high-key ranges, including a cold-object run that records bounded key-block reads |
| ordered limit | Repeated descending key-only `ORDER BY ... LIMIT` scans that must fetch no base tuples, warm and object-cold |
| parameterized join | Repeated 32-key nested-loop probes whose inner table must use its B-tree, warm and after a dedicated empty-local-cache restart |
| large catalog | Exact `regclass` resolution and `pg_class` lookup across a configurable schema of unrelated ordinary views, warm and after empty-local-cache recovery |
| warm disk | Graceful restart with the same disposable local data directory |
| empty local caches | Restart from a new local directory against the unchanged durable object prefix |
| concurrent updates | Synchronized hash-index target probes, commit latency, and immutable-batch/commit-head PUT amplification |
| checkpoint interference | Mixed reads and updates with and without overlapping explicit checkpoints |
| PostgreSQL 18 | Single/concurrent point reads, inserts, scans, and mixed-workload throughput through the same wire client |
| logical replicas | Aggregate reads across one through N durable subscribers, plus observed catch-up time |

Each workload reports attempted and completed operations, errors, elapsed
time, operations per second, p50/p95/p99/maximum/mean latency, process CPU,
peak RSS, RSS divided by the declared fixed memory plan, maintenance count,
table index/sequential scan deltas, and object requests and payload bytes by
PUT, full GET, ranged GET, LIST, and DELETE. The instrumented object-store
oracle speaks the same locked S3 profile as the other test endpoints. Its
optional metrics file and deterministic latency are test-process
instrumentation; production code never calls a private endpoint or a provider
branch.

## CI policy

CI runs the smoke suite and retains all raw artifacts. It gates zero errors
and complete operation counts, present and ordered percentiles, peak RSS no
more than 125% of the fixed plan, the stable object-operation metric schema,
at least one index scan per hash point-read, BRIN point or inclusion probe,
GiST inclusion, spatial, or K-nearest-neighbor probe, every GIN posting probe,
SP-GiST prefix, spatial, or K-nearest-neighbor probe, btree tail-range,
btree ordered-limit, parameterized-btree-join, or hash-targeted
synchronized-update operation and
zero sequential scans in the complete resident warm-memory paths, actual
shared-object reads during empty-local-cache recovery, and concurrent commit
PUT amplification below 1.75 PUTs per transaction. Ordered-limit runs also
require zero base-tuple fetches. These access-path gates keep a timing
improvement from concealing a return to full-table reads or updates. The
analyzed fixture has a fixed 8 KiB non-projected row body, so the
small smoke dataset spans enough immutable table blocks for a selective cold
index probe to remain a meaningful costed physical choice. The ordered-limit
fixture uses a descending btree with `INCLUDE (payload)` and projects both key
and payload, so its zero-fetch gate exercises durable covering-index behavior
rather than only key decoding. The GiST and SP-GiST K-nearest-neighbor fixtures
likewise carry their projected identifier and payload in the immutable index
generation; their warm and cold performance gates reject an added Sort or a
base-tuple fetch, while the cold-object correctness regression rejects
complete-generation reads for finite limits. The
ungrouped durable shape is two PUTs per transaction: an immutable journal
object and a compare-and-swap commit-head update.

Absolute timing is recorded but is not a hosted-runner gate. Stable regression
thresholds require pinned hardware and an independently operated compatible
object store; noisy CI timing is not evidence. Logical-replica speedup is also
reported rather than required to be linear.

## What the measurements decide next

Representative long runs, not the single-binary label or the small CI smoke
dataset, decide optimization order. Hash equality probes and btree equality
probes, including expression and implied partial-index keys, now use
resident exact maps or filtered immutable blocks for queries and direct DML.
Composite leading-prefix and range predicates seek across checkpoint-sorted
immutable keys and skip disjoint object blocks; the harness records both warm
and cold tail-range workloads alongside PostgreSQL 18. Compatible ordered
queries sort only compact index keys, stream base reads through `LIMIT`, and
avoid them entirely for key- and `INCLUDE`-covered projections. GiST range
overlap probes now use exact immutable-key predicate scans in warm and
empty-cache measurements. All four built-in GIN classes and SP-GiST text-prefix
probes have the same warm and empty-cache access-path gates. Built-in geometric
GiST and SP-GiST classes now provide PostgreSQL-compatible `<-> point` ordering over
compact immutable keys, including covering scans. Geometric predicates now
prune immutable bounding-box trees; the suite records their warm and cold
point-in-box and ranked-limit workloads independently. Three-level
fixed-allocation regressions and cold-read bounds qualify navigation without a
timing claim. Network, range, full-text,
GIN posting, and ranked nearest-neighbor navigation are now implemented.
Ranked GiST/SP-GiST limits retain only the requested window, use MVCC-safe
overlay-aware cutoffs, and rehydrate winning covering entries without reading
the complete generation. Residual-filter and locking shapes conservatively
retain complete exact ordering.
The known structural limits remain global query serialization and the
remaining per-object inline ceilings. Role, type, sequence, and ACL
catalog pools are startup-sized within their documented identity widths.
Checkpoint deletion markers likewise use the existing startup-sized row
overlay; crossing 1,024 deletes no longer forces a full-generation rewrite.
Split table-function output and effective search paths have no narrower
compiled row/entry count than their statement-memory and GUC byte boundaries.
Array producers likewise reserve their actual shape in statement memory up to
the durable 65,535-element boundary. Comparisons, searches, formatting,
`unnest`, casts, JSON conversion, and index token extraction walk encoded
array payloads sequentially, avoiding the quadratic prefix rescans that an
indexed lookup would impose on wide variable-length arrays.
Wide constraints, composites, partition definitions, and index tuples
already share their documented SQL, WAL, checkpoint, and recovery bounds.
Major SQL-object, metadata, database, and schema catalogs now have independent
startup-sized pools; database connection and statistics registries, checkpoint
row bookkeeping, and the named `checkpoint_manifest_bytes` reservation are
charged at startup as well. Commit-chain replay, the live-block garbage keep-set,
SST-pair merge scheduling, and garbage deletion batches are separately
startup-sized. Deletion is paced across batches rather than capped at one batch,
and live-block membership probes use a sorted fixed buffer instead of a linear
scan per listed object.
Multi-core execution must preserve fixed memory, MVCC, lock ordering, group
publication order, and explicit backpressure. Writer fencing and promotion
safety must exist before any failover benchmark or active-active claim is
meaningful.
