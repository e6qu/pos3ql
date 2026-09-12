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

## Running the suite

The smoke suite needs Rust, Python 3, and `nc`:

```sh
tools/run-performance.sh smoke /tmp/pos3ql-performance-smoke
```

The full suite additionally needs Docker unless
`POS3QL_BENCH_POSTGRES_PORT` names an existing PostgreSQL 18 instance:

```sh
tools/run-performance.sh full ./performance-results/local
```

The full run records Docker's resolved PostgreSQL image ID and `version()`
output, rather than treating a mutable image tag as provenance. Its defaults
can be overridden with `POS3QL_BENCH_ROWS`,
`POS3QL_BENCH_TABLE_CAPACITY`, `POS3QL_BENCH_OPERATIONS`,
`POS3QL_BENCH_CLIENTS`, `POS3QL_BENCH_REPLICAS`, and
`POS3QL_BENCH_OBJECT_LATENCY_MS`. The capacity must cover both the setup rows
and all rows inserted by the configured clients and operations.
CPU and resident-memory sampling uses `/proc` on Linux; peak RSS remains
available through `ps` on other supported systems.

The output directory contains an environment manifest with the commit, binary
hash, toolchain, machine, resources, and workload sizing; one raw JSON file
per workload; separate
recovery JSON intervals for initial/warm-disk/empty-local-cache starts, one
freshness interval per logical replica, the PostgreSQL 18 resolved image ID,
the pos3ql startup log with its fixed memory plan, and a derived `report.md`.

The PostgreSQL comparison covers the same SQL workload, concurrency, row
count, and operation count. PostgreSQL does not use pos3ql's durable-object
layout, so cache-tier and object-request measurements apply only to pos3ql;
the comparison does not pretend PostgreSQL itself has an S3 cache profile.

## Measured scenarios

| Scenario | Boundary measured |
|---|---|
| warm memory | Repeated point reads in the process that created and checkpointed the data |
| indexed tail range | Repeated selective high-key ranges, including a cold-object run that records bounded key-block reads |
| ordered limit | Repeated descending key-only `ORDER BY ... LIMIT` scans that must fetch no base tuples, warm and object-cold |
| warm disk | Graceful restart with the same disposable local data directory |
| empty local caches | Restart from a new local directory against the unchanged durable object prefix |
| concurrent updates | Synchronized clients, commit latency, and immutable-batch/commit-head PUT amplification |
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
at least one index scan per point-read, tail-range, ordered-limit, or synchronized-update operation and
zero sequential scans in the complete resident warm-memory paths, actual
shared-object reads during empty-local-cache recovery, and concurrent commit
PUT amplification below 1.75 PUTs per transaction. Ordered-limit runs also
require zero base-tuple fetches. These access-path gates keep a timing
improvement from concealing a return to full-table reads or updates. The
analyzed fixture has a fixed 8 KiB non-projected row body, so the
small smoke dataset spans enough immutable table blocks for a selective cold
index probe to remain a meaningful costed physical choice. The
ungrouped durable shape is two PUTs per transaction: an immutable journal
object and a compare-and-swap commit-head update.

Absolute timing is recorded but is not a hosted-runner gate. Stable regression
thresholds require pinned hardware and an independently operated compatible
object store; noisy CI timing is not evidence. Logical-replica speedup is also
reported rather than required to be linear.

## What the measurements decide next

Representative long runs, not the single-binary label or the small CI smoke
dataset, decide optimization order. Plain-column btree equality probes now use
resident exact maps or filtered immutable blocks for queries and direct DML.
Composite leading-prefix and range predicates seek across checkpoint-sorted
immutable keys and skip disjoint object blocks; the harness records both warm
and cold tail-range workloads alongside PostgreSQL 18. Compatible ordered
queries sort only compact index keys, stream base reads through `LIMIT`, and
avoid them entirely for key-covered projections. The known structural limits
remain global query serialization, richer secondary-index planning, and small
startup-sized catalog/table ceilings. Multi-core execution must preserve fixed memory, MVCC,
lock ordering, group publication order, and explicit backpressure. Writer
fencing and promotion safety must exist before any failover benchmark or
active-active claim is meaningful.
