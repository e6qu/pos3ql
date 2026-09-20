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

The baseline runs actual, unmodified PostgreSQL 18 with its ordinary local
storage and durability settings. Both systems receive the same SQL workload,
concurrency, row count, and operation count. pos3ql instead publishes durable
state to object storage. A representative comparison must record PostgreSQL's
storage medium and settings alongside pos3ql's object store, network, and cache
conditions. End-to-end latency and throughput can be compared directly for the
stated setups; storage request, cache-tier, and recovery measurements describe
each system's different persistence design and must be reported separately.

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
