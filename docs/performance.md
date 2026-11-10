# Measuring performance and resource use

The benchmark suite runs the same PostgreSQL client against pos3ql and
PostgreSQL 18. Each run records the commit, binary, workload, response times,
completed work and resource use in versioned JSON files;
`tools/benchmark-report.py` builds a report from them.

**No representative deployment has been measured yet.** Existing results use
an instrumented fixture or MinIO/SeaweedFS on the same host. They show tested
behavior and exploratory costs, not production speed, a maximum database size
or a provider ranking. The work to change that is
[measuring a first deployment baseline](../PLAN.md#measure-a-first-deployment-baseline)
and [qualifying production deployment](../PLAN.md#qualify-production-deployment).

## What the numbers can tell you

**User example:** an orders screen makes many small lookups while background
work updates orders. Measure both the usual response time and the slow tail,
with and without checkpoints. A p95 of 20 ms means 95% of measured requests
finished within 20 ms; p99 is the boundary of the slowest 1%. A good average
can hide requests that users experience as stalls.

| Your question | Measure |
|---|---|
| How quickly can users look up a cached order? | Warm-memory and warm-disk reads |
| What happens after losing local caches? | Empty-cache recovery and first object reads |
| Do background jobs stall users? | Mixed reads/writes with and without checkpoints |
| Does an index reduce storage work? | Access paths, object requests and bytes fetched |
| Does the layout avoid expensive rewrites? | Bytes written for updates and maintenance relative to changed data |
| Will more connections help? | Completed work, latency, errors, CPU and memory as concurrency changes |
| How long until a replica shows an update? | Observed subscriber catch-up time |

Every run compares against two vanilla PostgreSQL 18 controls on ordinary
local durable storage:

- **host-available:** no container CPU or memory limit;
- **resource-matched:** Docker limited to the CPUs available to pos3ql and to
  pos3ql's exact fixed startup memory plan, with memory and memory-plus-swap
  limits equal so it cannot borrow swap. The report validates these limits from
  Docker metadata.

Both receive the same SQL workload, concurrency, row count and stopping rule;
duration-bound runs can complete different operation counts. End-to-end latency
and throughput are directly comparable for the stated setups. Storage requests,
cache tiers and recovery describe each system's different persistence design and
are reported separately.

## Estimating bandwidth and throughput

A SQL statement is a command such as SELECT or UPDATE; a transaction can
contain several statements and one commit. The benchmark's `ops/s` is its
workload operation count divided by elapsed time. It is not automatically
statements or committed transactions per second. State which unit you measured,
how errors count, and whether connection and commit time are included.

[Architecture](architecture.md#memory-and-physical-size) lists hard physical
limits and configurable inventories. Do not infer a petabyte database or
unlimited throughput from object-store capacity alone.

**Reads.** If average request occupancy is `L` seconds, a read transfers `S`
bytes, `C` request slots are usable and link bandwidth is `B` bytes/second, an
optimistic ceiling is:

```text
completed object reads/second <= min(C / L, B / S)
useful query rate <= completed reads/second / uncached reads per query
```

This excludes CPU, signing/TLS, metadata reads, queueing, retries, contention,
maintenance and protocol traffic. Smaller ranged reads change `S`; cache hits
avoid object requests. More slots hide latency until another limit dominates.

*Calculated example, not a measurement:* four read slots, 20 ms occupancy per
256 KiB response and 100 MiB/s for data. The slot bound is `4 / 0.020 = 200`
reads/s; the bandwidth bound is `100 MiB / 256 KiB = 400` reads/s. The smaller
bound gives at most 50 MiB/s. At eight uncached reads per query, the storage
ceiling is 25 queries/s before CPU and other traffic.

**Commits.** Journal bytes and their descriptor must be durable before the
conditional commit-head update. Grouping transactions amortizes that work but
does not remove ordered head publication.

*Calculated example:* the two uploads overlap and each phase, including the
head update, takes 20 ms, with no overlap between batches and negligible
transfer/CPU cost. That is at least 40 ms per batch, at most 25 batches/s. With
`commit_queue_slots` of at least eight and eight transactions per batch, that is
an optimistic 200 committed transactions/s; one per batch gives 25. The server
promises neither these rates nor any batch occupancy.

**CPU.** SQL runs on one CPU worker. If processing costs an assumed 2 ms per
operation, that worker completes at most 500 operations/s before maintenance
and scheduling. Network and CPU ceilings cannot simply be added.

## What the retained measurements actually show

The [2026-09-23 resource-matched matrix](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/benchmarks/baselines/2026-09-23-resource-matched-postgresql-10000/report.md)
records revision `90861f1bb8b464b8032e79fac8d6a28681aaa9ba`, 10,000 rows, four
clients, 12 available CPUs and a 949.81 MiB fixed pos3ql plan, with MinIO and
SeaweedFS on the same host.

| Historical workload | pos3ql ops/s | pos3ql p99 | Matched PostgreSQL ops/s | Matched PostgreSQL p99 |
|---|---:|---:|---:|---:|
| MinIO, mixed with checkpoints | 130.43 | 557.86 ms | 3,152.76 | 10.05 ms |
| SeaweedFS, mixed with checkpoints | 130.27 | 441.96 ms | 3,488.84 | 7.33 ms |

These expose a substantial gap in that old setup. They do not measure the
current code, and they do not show that either provider is faster.
[The benchmark index](../benchmarks/README.md) lists all retained runs;
[implementation history](history/README.md) keeps the checkpoint optimization
chronology.

## Running the suite

Run full builds and large benchmark runs on dedicated or GitHub runners, as
described in [contributing](../CONTRIBUTING.md#development-and-resource-policy).

```sh
tools/run-performance.sh [smoke|full|checkpoint] [output-directory]
```

| Mode | Needs | Defaults |
|---|---|---|
| `smoke` | Rust, Python 3, `nc` | Fixed: 128 rows, capacity 512, 8 operations, 4 clients, no replicas, 16 catalog views |
| `full` | Also Docker | 10,000 rows, capacity 16,384, 500 operations, 8 clients (minimum 4), 2 replicas, 128 catalog views |
| `checkpoint` | Also Docker | As `full`, but only setup and mixed checkpoint pressure; requires zero replicas and zero catalog views |

`full` needs at least four clients so the synchronized update workload can
enforce the group-commit amplification gate. Table capacity must cover the setup
rows plus every row the clients insert.

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_DISK_CACHE_MIB=1024 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint
```

### Settings

| Variable | Meaning |
|---|---|
| `POS3QL_BENCH_ROWS`, `_TABLE_CAPACITY`, `_OPERATIONS`, `_CLIENTS`, `_REPLICAS` | Workload sizing (ignored by `smoke`) |
| `POS3QL_BENCH_CATALOG_RELATIONS` | Ordinary views in an isolated schema, for relation-name and `pg_class` lookup as the catalog grows |
| `POS3QL_BENCH_DISK_CACHE_MIB` | Fixed local disk block cache (default 128) |
| `POS3QL_BENCH_TIMEOUT_SECONDS` | Per-query socket timeout (default 30). A timeout is a failed measurement to investigate, not one to fix by extending the deadline |
| `POS3QL_BENCH_OBJECT_STORE` | `fixture` (default), `minio`, `seaweedfs` or `external` |
| `POS3QL_BENCH_OBJECT_LATENCY_MS` | Fixture-only injected request latency (default 2; must be 0 elsewhere) |
| `POS3QL_BENCH_MINIO_IMAGE`, `_SEAWEEDFS_IMAGE` | Override the pinned image digests; Docker's resolved image ID is recorded |
| `POS3QL_BENCH_POSTGRES_PORT` | Use an existing PostgreSQL 18 instance as the host-available control (the matched control still runs in Docker) |
| `POS3QL_BENCH_POSTGRES_STORAGE` | Describes that instance's storage medium |
| `POS3QL_BENCH_CHECKPOINT_PROFILE` | `1` builds the checkpoint profiling feature (`checkpoint` mode only) |

The PostgreSQL controls must run PostgreSQL 18 with `fsync`,
`full_page_writes` and `synchronous_commit` on. The harness records
`version()`, durability and resource settings, and, for Docker, the resolved
image ID and mounts rather than a mutable tag.

### Object-store matrix

```sh
tools/run-performance-matrix.sh [checkpoint|full] ./performance-results/object-matrix
```

This runs the fixture, MinIO and SeaweedFS in turn, each with both PostgreSQL
controls, and rejects mismatched commits, binaries or workload shapes in the
combined report. The fixture retains exact request and byte counters. MinIO and
SeaweedFS run without injected latency and report request metrics as
unavailable rather than estimating them. They are independent S3-compatible
implementations on the benchmark host, not independently operated services.

### External object storage

The `external` profile is for the representative run against an existing,
independently operated S3-compatible service. It requires TLS, an existing
bucket, credentials with read, write, list and delete access, and explicit
descriptions of the service, infrastructure and storage media:

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

- The endpoint is a `host:port` authority, not a URL.
- `POS3QL_BENCH_OBJECT_STORE_ADDRESSING` is `path` (default) or
  `virtual_hosted`.
- Optional: `POS3QL_BENCH_OBJECT_STORE_SESSION_TOKEN` for temporary
  credentials, `POS3QL_BENCH_OBJECT_STORE_TLS_CA_FILE` for a private trust root.
- Each run gets a unique generated prefix; set `POS3QL_BENCH_OBJECT_STORE_PREFIX`
  only for a caller-managed unique namespace. Objects stay there so cold recovery
  is reproducible; remove them after retaining the artifacts.
- Keys, secrets, session tokens and trust-root contents are never written to
  artifacts; a custom trust root is identified only by its SHA-256 digest.
- Provider request counters are unavailable unless the provider supplies
  separate evidence.

### Output

The output directory contains:

- an environment manifest: commit, binary hash, toolchain, machine, resources,
  workload sizing, disk cache size, timeout and catalog view count;
- one raw JSON file per workload, recovery JSON for initial, warm-disk and
  empty-local-cache starts, and one freshness interval per logical replica;
- `postgresql-server.json` and `postgresql-matched-server.json` with
  PostgreSQL settings, storage description and container limits;
- the pos3ql startup log with its fixed memory plan, and a derived `report.md`;
- on failure, server logs in `server-logs/`, workload errors and the host
  inventory.

Recovery timing ends when the process reports startup complete and its SQL
listener accepts connections, including restoration from shared storage. CPU
and resident-memory sampling use `/proc` on Linux; other systems report peak
RSS through `ps`.

## Measured scenarios

“Warm” means data is already cached; “object-cold” means the path must read
from object storage. A “covering” index contains the requested values, so
eligible reads need not fetch the table row. These are tested paths, not a
promise that every query uses them.

| Scenario | Boundary measured |
|---|---|
| warm memory | Repeated hash-index point reads in the process that created and checkpointed the data |
| BRIN point pruning | Warm and object-cold equality probes through a BRIN key and bitmap plan |
| BRIN inclusion filtering | Warm and object-cold range-overlap probes through `range_inclusion_ops` and a bitmap plan |
| GiST inclusion filtering | Warm and object-cold range-overlap probes through a GiST key generation |
| GiST geometric pruning | Warm and object-cold point-in-box probes through immutable bounding-box navigation |
| GiST K-nearest-neighbor | Warm and object-cold `<-> point` ordered limits through a covering GiST generation |
| GIN array postings | Warm and object-cold array-containment probes |
| GIN posting unions | Warm and object-cold multi-token array-overlap probes, including an absent token |
| GIN full-text postings | Warm and object-cold lexeme probes |
| GIN `jsonb_ops` postings | Warm and object-cold top-level existence probes |
| GIN `jsonb_path_ops` postings | Warm and object-cold containment probes |
| SP-GiST prefix filtering | Warm and object-cold text-prefix probes |
| SP-GiST geometric pruning | Warm and object-cold point-in-box probes |
| SP-GiST K-nearest-neighbor | Warm and object-cold `<-> point` ordered limits through a covering k-d point generation |
| indexed tail range | Selective high-key ranges, including a cold run that records bounded key-block reads |
| ordered limit | Descending key-only `ORDER BY ... LIMIT` scans that fetch no base tuples, warm and object-cold |
| parameterized join | 32-key nested-loop probes whose inner table uses its B-tree, warm and after an empty-local-cache restart |
| large catalog | `regclass` resolution and `pg_class` lookup among many unrelated views, warm and after empty-local-cache recovery |
| warm disk | Graceful restart with the same local data directory |
| empty local caches | Restart from a new local directory against the unchanged object prefix |
| concurrent updates | Synchronized hash-index target probes, commit latency and journal/commit-head PUT amplification |
| checkpoint interference | Mixed reads and updates with and without overlapping explicit checkpoints |
| PostgreSQL 18 | Single and concurrent point reads, inserts, scans and mixed throughput through the same wire client |
| logical replicas | Aggregate reads across one through N subscribers, plus observed catch-up time |

Each workload reports attempted and completed operations, errors, elapsed time,
operations per second, p50/p95/p99/maximum/mean latency, process CPU, peak RSS,
RSS divided by the fixed memory plan, maintenance count, index/sequential scan
deltas, and object requests and payload bytes by PUT, full GET, ranged GET, LIST
and DELETE. The instrumented fixture speaks the same S3 profile as other test
endpoints; its metrics file and injected latency are test instrumentation, and
production code never calls a private endpoint.

## Measuring shared startup capacity

On a CI runner, with a new output directory:

```sh
python3 tools/measure-embedded-capacity.py --output NEW_DIRECTORY \
  --budget-limit-bytes 2147483648
```

The 2 GiB ceiling rejects an oversized reservation; it does not allocate 2 GiB.
The 64-sample matrix fixes equal explicit `row_overlay_slots`,
`row_work_bytes`, `row_work_leases`, `definition_image_slots`,
`execution_control_bytes` (256 KiB), `execution_control_leases` (256),
`table_metadata_bytes` (64 MiB), `table_metadata_leases` (8,192),
`statistics_bytes` (1 MiB), `statistics_leases` (1,024) and
`index_statistics_slots` (4,096) across 1/8/32/128 configured tables in both
transaction modes. Each combination runs:

- empty, one-hot-table, evenly distributed, moving-demand, wide-schema (64
  nullable columns) and replacement workloads; replacement creates a unique
  index, adds a column, rolls back to a savepoint, then drops and recreates the
  index and checks row counts and sums;
- two statistics profiles that spread rows across all tables and analyze every
  column with two-column and 64-column schemas; the wide one also changes a
  column type, analyzes it and rolls back to a savepoint. Older samples lack
  these profiles.

Metadata, statistics and index-counter backing and their free-range controls
must stay constant across table counts. The output keeps the exact commands,
revision, compiler, binary digest, named budget draws and raw JSON with
checksums. The script runs the helper's metric tests before building.

**Resident memory (RSS metric version 2).** Every sample names its source.

| Field | Linux source | macOS source |
|---|---|---|
| `startup_peak_rss_bytes`, `final_peak_rss_bytes` | `VmHWM` in `/proc/self/status` | `getrusage`, labelled `darwin_getrusage_ru_maxrss` |
| `startup_current_rss_bytes`, `final_current_rss_bytes` | `VmRSS` | `null` |
| `startup_getrusage_maxrss_bytes`, `final_getrusage_maxrss_bytes` | `getrusage` diagnostic | — |

The helper reads `/proc` into a fixed 8 KiB buffer, converts `kB` by
multiplying by 1,024, and rejects missing fields, invalid units, overflow or
oversized output rather than substituting another counter. These are kernel RSS
snapshots, not allocator byte counts. On Linux,
[exec preserves the previous address space's peak](https://github.com/torvalds/linux/blob/v7.0/fs/exec.c#L838)
for `getrusage`, while
[the proc counters use the current address space](https://github.com/torvalds/linux/blob/v7.0/fs/proc/task_mmu.c#L34),
so a launcher using `vfork` or `CLONE_VM` can impose its own earlier peak.
`tools/tests/test_measure_embedded_capacity.py` demonstrates this with a 32 MiB
allocate/free launch. Earlier unlabelled `getrusage` peaks are not engine-only
memory and are not comparable to version 2; their named budget components remain
valid. CI runs on Linux, and a new resident-memory claim needs fresh version-2
samples.

Compare named shared pools at equal settings. Total memory still rises with
table identities, catalog and history structures, connection metadata and other
independent reservations. Peak RSS counts touched pages, which differs from
reserved bytes and from the 128 MiB virtual query stack. Populated samples
report a separate session reservation; empty samples start no session. This
single-session matrix does not measure retained-reader pressure, object-store
throughput or an embedded production minimum.

## CI policy

CI checks correct answers, complete runs, memory use and intended access paths.
For example, an indexed order lookup must not become a full-table scan that
still looks fast on a tiny dataset. Hosted-runner timing is recorded but never
a pass/fail threshold: stable timing thresholds need pinned hardware and an
independently operated object store. Logical-replica speedup is reported, not
required to be linear.

CI runs the smoke suite, retains all raw artifacts, and requires:

- zero errors and complete operation counts;
- present, ordered percentiles and the stable object-operation metric schema;
- peak RSS at most 125% of the fixed memory plan;
- at least one index scan per operation, and zero sequential scans in the
  resident warm-memory paths, for hash point reads, BRIN, GiST, GIN and SP-GiST
  probes, B-tree tail ranges, ordered limits, parameterized joins and
  hash-targeted synchronized updates;
- zero base-tuple fetches for ordered limits and for the GiST and SP-GiST
  K-nearest-neighbor fixtures, and no added Sort for the latter;
- actual object reads during empty-local-cache recovery;
- concurrent commit amplification below 1.75 PUTs per transaction.

The ungrouped durable shape is three PUTs per transaction: journal bytes, their
descriptor, and a compare-and-swap commit-head update published after both
uploads complete. The analyzed fixture has a fixed 8 KiB row body so the small
smoke dataset spans enough immutable blocks for a selective cold index probe to
be a meaningful costed choice. The ordered-limit fixture uses a descending
B-tree with `INCLUDE (payload)` and projects both columns, so its zero-fetch
gate exercises durable covering-index reads. A separate cold-object correctness
regression rejects reading complete K-nearest-neighbor index generations for
finite limits.
