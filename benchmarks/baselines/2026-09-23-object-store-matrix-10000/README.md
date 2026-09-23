# 10,000-row object-store matrix

This clean checkpoint matrix was collected from harness commit
`7171836a1bc60d3ef2125ccacc3fee80803d8761` and pos3ql binary SHA-256
`779e19713c2a5ea0b8c7881b35b895fbd5a9f5d9ef92c12cb82b41d19c6c150c`.
Every leg used 10,000 setup rows, four clients, an eight-second minimum mixed
workload, three explicit checkpoints, a 2 GiB fixed disk cache, and checkpoint
phase tracing. Each object-store leg includes a separately measured vanilla
PostgreSQL 18.6 baseline with `fsync`, `full_page_writes`, and
`synchronous_commit` enabled on its recorded Docker-managed local volume.

The pos3ql durable tiers were:

- the instrumented local fixture with 2 ms artificial latency and exact
  request counters;
- MinIO `RELEASE.2025-09-07T16-13-09Z` through its pinned image digest, with
  no artificial latency; and
- SeaweedFS 4.46 through its pinned image digest, with no artificial latency.

MinIO and SeaweedFS ran in local Docker containers. Their environment files
record both the immutable image reference and resolved image ID. They are
independent S3-compatible implementations, but they are neither remote nor
independently operated object-storage services. Their native request counters
are unavailable to this harness, so the raw workload and recovery artifacts
use `null` and the reports render an em dash rather than estimating them.

All 18 measured engine/scenario combinations completed without errors. The
headline mixed-workload results were:

| Backend | Engine | Baseline ops/s | Baseline p99 ms | Checkpoint ops/s | Checkpoint p99 ms |
|---|---|---:|---:|---:|---:|
| fixture | pos3ql | 206.83 | 76.77 | 100.14 | 758.77 |
| fixture | PostgreSQL 18 | 2,312.31 | 8.91 | 2,145.72 | 10.96 |
| MinIO | pos3ql | 157.15 | 103.34 | 75.21 | 1,948.90 |
| MinIO | PostgreSQL 18 | 2,557.54 | 8.23 | 2,935.24 | 6.82 |
| SeaweedFS | pos3ql | 154.35 | 114.11 | 46.94 | 2,087.74 |
| SeaweedFS | PostgreSQL 18 | 1,787.24 | 14.02 | 1,629.75 | 14.94 |

The fixture, MinIO, and SeaweedFS checkpoint windows completed 803, 602, and
400 foreground operations, respectively. Their row-merge writers produced 58,
497, and 494 block PUTs over 235, 237, and 218 events. Those different change
volumes and generation shapes, together with variation in the three sequential
PostgreSQL baselines, prevent a controlled provider ranking. The run establishes
that the paired harness works against all three implementations and records the
large checkpoint-overlap cost on each stated setup. Representative claims still
require pinned hardware and an independently operated object store.

The combined [report](report.md) presents completed operation counts, latency,
throughput, request availability, and paired ratios. Each backend directory
contains its schema-versioned raw workload and phase data, complete environment
and container provenance, startup log, PostgreSQL settings, and derived report.

Reproduce the matrix with:

```sh
POS3QL_BENCH_ROWS=10000 POS3QL_BENCH_TABLE_CAPACITY=16384 \
POS3QL_BENCH_OPERATIONS=100 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_DISK_CACHE_MIB=2048 POS3QL_BENCH_TIMEOUT_SECONDS=300 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 POS3QL_BENCH_CHECKPOINT_SECONDS=8 \
POS3QL_BENCH_REPLICAS=0 \
tools/run-performance-matrix.sh checkpoint ./performance-results/object-matrix-10000
```
