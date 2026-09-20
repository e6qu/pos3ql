# Paired checkpoint overlap with PostgreSQL 18, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw workloads](mixed-checkpoint-interference.json), [startup log](pos3ql-startup.log),
and [derived report](report.md) preserve a focused run from clean commit
`0821645dcc49e6110a2ef60f6a2ccc309e1ca7e6`. The manifest records the
release binary SHA-256, host, toolchain, cache, and timeout. PostgreSQL 18.6
was an unmodified Homebrew build in an isolated temporary data directory on
local APFS. Its `fsync`, `full_page_writes`, and `synchronous_commit` settings
were on. Its data directory was removed after the run.

Both engines received the same schema, 1,000 setup rows, four clients, 50
mixed SQL operations per client, and three explicit `CHECKPOINT` commands
with a 1 ms maintenance interval. The harness rejects a run that completes
fewer than three checkpoints. The baseline workloads issued the same mixed
operations without explicit checkpoints. All four workloads completed without errors;
the raw JSON records operation and checkpoint counts.

| Engine | Baseline p99 | Checkpoint overlap p99 | Baseline elapsed | Checkpoint overlap elapsed |
|---|---:|---:|---:|---:|
| pos3ql | 72.39 ms | 1,447.94 ms | 0.77 s | 2.41 s |
| PostgreSQL 18 | 2.76 ms | 1.51 ms | 0.011 s | 0.013 s |

pos3ql's checkpoint workload made 271 object PUTs and 202 DELETEs; its
baseline made 84 PUTs and no DELETEs. PostgreSQL uses its local storage and
WAL, so it has no comparable object-request counts. The PostgreSQL p99 samples
vary across these very short runs and do not establish a checkpoint effect.
These are exploratory timings on one unreserved Apple Silicon macOS 15.6.1
host with 12 logical CPUs and 24 GiB RAM. pos3ql used a 1 GiB fixed disk
cache and an instrumented S3-compatible fixture backed by temporary local
storage with 2 ms injected request latency. The systems' distinct persistence
costs are reported separately; this is not a production performance ratio.

Reproduce with an isolated PostgreSQL 18 server configured as recorded in
`postgresql-server.json`, then run:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 POS3QL_BENCH_POSTGRES_PORT="$PG_PORT" \
POS3QL_BENCH_POSTGRES_STORAGE='Local APFS, isolated PostgreSQL 18 data directory; fsync enabled' \
tools/run-performance.sh checkpoint ./performance-results/postgresql18-checkpoint-1000
```
