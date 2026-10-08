# Performance and scaling boundary

pos3ql has a reproducible measurement harness, not a blanket production
performance claim. The same dependency-free PostgreSQL v3 client drives
pos3ql and PostgreSQL 18; every run records database identity, workload shape,
latency and throughput summaries, and resource evidence as schema-versioned
JSON. `tools/benchmark-report.py` derives a report from those raw files.

## Current topology

- One server process owns one writable database state and
  serializes query execution. Startup-sized pools bound memory and make
  saturation an explicit error. `query_workspace_slots` reserves independent
  execution arenas and ordinary DML row-selection buffers. The dispatcher
  leases both together through an exactly charged FIFO pool. The current
  reactor still executes one statement at a time, so additional slots provide
  state isolation and memory accounting rather than a throughput claim.
  Logical subscription apply and bootstrap COPY use their worker-owned arena
  and DML scratch instead of a client workspace.
  Long-lived COPY streams keep transition rows in their connection's fixed
  startup buffer, so interleaved clients do not share statement state.
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
- One writer incarnation owns an object prefix at a time. Starting another
  process promotes it through the durable writer fence and causes later
  publishes by the displaced process to fail. The packaged passive-candidate
  monitor starts that promotion after bounded readiness failures. Ownership
  leases and a read-only shared-snapshot protocol do not exist.
- Eligible two-source equi-joins use a bounded hash build for physical tables,
  synthesized catalogs, and derived tables, including external runs. NULL keys,
  duplicate matches, residual ON predicates, and LEFT JOIN preservation share
  one execution path. The fixed build-entry ceiling still bounds eligibility;
  larger builds choose a nested-loop plan before execution.
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

## Retained evidence

[The benchmark index](../benchmarks/README.md) links the original raw runs and
reports, including the fixture/MinIO/SeaweedFS matrix with both vanilla
PostgreSQL controls. The [historical notes](history/README.md) preserve the
checkpoint optimization chronology. These shared-host runs establish
implementation and request-shape evidence; differing operation counts and
generation shapes prevent a controlled production timing ratio.

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

## Remaining qualification

Execution remains serial. The [roadmap](../PLAN.md#remaining-sequence) defines
the shared-state and worker prerequisites, representative deployment inputs,
and required scaling, recovery, freshness, and failover evidence. Timing claims
require repeated long runs on pinned hardware with independent object storage;
retain both PostgreSQL controls and each system's actual persistence settings.
