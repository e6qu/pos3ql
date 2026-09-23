# 10,000-row resource-matched PostgreSQL matrix

This clean checkpoint matrix was collected from harness commit
`90861f1bb8b464b8032e79fac8d6a28681aaa9ba` and pos3ql binary SHA-256
`779e19713c2a5ea0b8c7881b35b895fbd5a9f5d9ef92c12cb82b41d19c6c150c`.
Every leg used 10,000 setup rows, four clients, an eight-second minimum mixed
workload, three explicit checkpoints, a 2 GiB fixed disk cache, and checkpoint
phase tracing.

Each fixture, MinIO, and SeaweedFS leg includes two actual PostgreSQL 18.6
controls from the same stock image:

- the host-available control has no Docker CPU or memory limit; and
- the resource-matched control has a 12-CPU quota and a 995,951,270-byte
  (949.81 MiB) memory limit, matching the CPU count available to pos3ql and its
  recorded fixed startup memory plan. Its memory-plus-swap limit is the same
  995,951,270 bytes, preventing swap borrowing.

The harness read the effective limits back from Docker and rejected unequal
values. Both controls used image ID
`sha256:d8a40176c29aa0c7a20a19f85ddddc47f72d8d6789a7a86a21f3713e71fb4ad6`
with `fsync`, `full_page_writes`, and `synchronous_commit` enabled. PostgreSQL
kept its normal database configuration and Docker-managed local durable tier.
The memory comparison gives PostgreSQL a whole-container cgroup ceiling equal
to pos3ql's fixed allocation plan; the two measurements describe different
memory accounting boundaries.

The pos3ql durable tiers were the instrumented fixture with 2 ms artificial
latency, pinned local MinIO, and pinned local SeaweedFS. MinIO and SeaweedFS
had no artificial latency. They are independent S3-compatible implementations
running on the benchmark host, rather than independently operated object
storage services. Native provider request counters remain unavailable.

All 27 measured engine and scenario combinations completed without error. The
headline mixed-workload results were:

| Backend | Engine | Baseline ops/s | Baseline p99 ms | Checkpoint ops/s | Checkpoint p99 ms |
|---|---|---:|---:|---:|---:|
| fixture | pos3ql | 202.10 | 83.16 | 215.23 | 56.13 |
| fixture | PostgreSQL 18 host available | 6,977.88 | 3.54 | 6,241.95 | 4.23 |
| fixture | PostgreSQL 18 resource matched | 6,442.94 | 4.11 | 6,394.09 | 3.98 |
| MinIO | pos3ql | 215.30 | 126.19 | 130.43 | 557.86 |
| MinIO | PostgreSQL 18 host available | 4,102.04 | 7.71 | 3,543.78 | 7.72 |
| MinIO | PostgreSQL 18 resource matched | 1,939.32 | 10.72 | 3,152.76 | 10.05 |
| SeaweedFS | pos3ql | 276.55 | 60.25 | 130.27 | 441.96 |
| SeaweedFS | PostgreSQL 18 host available | 3,765.74 | 7.21 | 3,288.44 | 8.86 |
| SeaweedFS | PostgreSQL 18 resource matched | 3,660.41 | 7.49 | 3,488.84 | 7.33 |

The resource cap materially changed one MinIO PostgreSQL baseline sample and
had smaller or reversed effects in other sequential samples. Different
duration-bound foreground counts, cache state, and checkpoint generation
shapes prevent interpreting those differences as a controlled CPU or memory
effect. PostgreSQL also uses local persistence while pos3ql publishes to an
object store. This shared-host run establishes executable comparison coverage;
it does not establish production ratios or rank object-store implementations.

The combined [report](report.md) presents resource parity, completed operation
counts, latency, throughput, request availability, and ratios against both
PostgreSQL controls. Each backend directory contains the raw workload and
checkpoint profile, environment and container provenance, startup log,
PostgreSQL settings and limits, and a derived report.

Reproduce the matrix with:

```sh
POS3QL_BENCH_ROWS=10000 POS3QL_BENCH_TABLE_CAPACITY=16384 \
POS3QL_BENCH_OPERATIONS=100 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_DISK_CACHE_MIB=2048 POS3QL_BENCH_TIMEOUT_SECONDS=300 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 POS3QL_BENCH_CHECKPOINT_SECONDS=8 \
POS3QL_BENCH_REPLICAS=0 \
tools/run-performance-matrix.sh checkpoint ./performance-results/object-matrix-10000
```
