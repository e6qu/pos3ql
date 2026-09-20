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
