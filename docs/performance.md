# Performance and scaling boundary

pos3ql does not yet have evidence for a production performance claim. The
single executable avoids a storage gateway and talks directly to the common
S3-compatible HTTP data-plane API, but process shape alone does not establish
throughput or latency.

## Current topology

- One server process owns one writable database state and currently
  serializes query execution. Startup-sized pools bound memory and make
  saturation an explicit error.
- Object storage is durable; memory and local disk are caches. Commit batches
  and checkpoint manifests amortize object requests, but cold reads,
  compaction, and object latency still need measurement.
- Logical publications and subscriptions can build independently durable read
  copies and interoperate with PostgreSQL. They are asynchronous logical
  replicas, not transparent shared-storage replicas.
- Starting several writable processes on one object prefix is unsupported.
  Writer fencing, ownership leases, failover, and a read-only shared-snapshot
  protocol do not exist yet. Multiple processes therefore do not provide
  safe active-active or automatic read scaling.

## Measurement sequence

The baseline must use pinned binaries, hardware, object-store implementation,
dataset, configuration, and workload seed, and retain raw results. Report at
least operations per second; p50, p95, p99, and maximum latency; CPU time;
fixed-memory occupancy and exhaustion; object requests and bytes by operation;
cache hit rates; checkpoint/compaction overlap; replication lag; and recovery
time.

1. Measure one process at concurrency 1, then increase connections until
   throughput plateaus or a bounded resource rejects work. Separate point
   reads, indexed reads, inserts, updates, mixed OLTP, COPY ingest, scans,
   joins, grouping, sorting, and spill.
2. Repeat each workload with warm memory/local-disk caches, cold memory with a
   warm disk cache, and both caches empty. Inject fixed object latency and
   bandwidth limits and count request amplification.
3. Overlap the same workloads with commit publication, checkpointing,
   compaction, and garbage collection. Measure tail latency and backpressure,
   not only average throughput.
4. Benchmark one writer plus one through N logical read replicas. Report
   aggregate read throughput and freshness/lag separately. A load balancer is
   external to this measurement and must not route writes to replicas.
5. Only after writer fencing and failover exist, measure promotion time,
   acknowledged-commit safety, stale-writer rejection, and recovery with empty
   local caches. Active-active claims require a separate consistency design;
   they cannot be inferred from logical-replica throughput.

The first expected bottleneck is global execution serialization; the next
likely boundaries are physical secondary-index access, object-request
amplification under cold reads, and the current small fixed catalog/table
ceilings. Measurements, rather than the single-binary label, decide their
order.
