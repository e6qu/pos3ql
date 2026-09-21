# Checkpoint foreground correlation, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
feature-enabled run from clean commit
`979cd537e458e3416683d5f1a62dde9de1628a72`. The manifest records the
release binary SHA-256, host, toolchain, cache, four-second minimum workload
duration, and 120-second query timeout.

The mixed workload had 1,000 setup rows, four clients, and three explicit
checkpoints. All 200 pos3ql checkpoint-overlap operations and 73,310
PostgreSQL 18 checkpoint-overlap operations completed without errors. Their
operation counts differ because each workload ran for at least four seconds.
The harness releases checkpoint maintenance after the foreground worker
barrier and rejects a profile with no observed foreground overlap.

| pos3ql phase | Phase time | Summed foreground intersection | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|
| Value-index rebuild | 4,411 ms | 17,645 ms | 754 | 0 |
| Row SST publication | 3,307 ms | 13,227 ms | 718 | 0 |
| Block garbage deletion | 2,463 ms | 6,739 ms | 0 | 768 |
| Commit-batch pruning | 1,461 ms | 5,790 ms | 0 | 418 |
| Local cleanup | 0.81 ms | 3.18 ms | 0 | 0 |

The foreground intervals use realtime only to align the client and server
processes; each interval's duration comes from its process's monotonic clock.
Summed intersections include concurrent operations and can exceed phase wall
time. One operation can intersect several sequential phases. The association
shows where client waits coincide with checkpoint work without claiming that
each intersected millisecond is exclusively caused by that phase.

The profile window includes automatic checkpoint work and cleanup through
server stop as well as the three explicit commands. It recorded 1,621 object
PUTs, 1,186 DELETEs, 36 LISTs, and 289 ranged GETs. Phase counters account for
all DELETEs and 1,472 block PUTs. The remaining PUTs include commit and
manifest publication outside the block-writer phases.

The host was an unreserved Apple Silicon macOS 15.6.1 machine with 12 logical
CPUs and 24 GiB RAM. pos3ql used a 1 GiB fixed disk cache and an instrumented
S3-compatible fixture backed by temporary local storage with 2 ms injected
request latency. Actual PostgreSQL 18.6 used an isolated local APFS data
directory with `fsync`, `full_page_writes`, and `synchronous_commit` on.
pos3ql p99 was 76.77 ms without explicit checkpoints and 2,548.02 ms in the
checkpoint-overlap workload. PostgreSQL p99 was 0.52 ms and 0.54 ms. These
shared-host timings are exploratory and the systems use distinct persistence
tiers.

Reproduce with an isolated PostgreSQL 18 server configured as recorded in
`postgresql-server.json`, then run:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 POS3QL_BENCH_CHECKPOINT_SECONDS=4 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 POS3QL_BENCH_POSTGRES_PORT="$PG_PORT" \
POS3QL_BENCH_POSTGRES_STORAGE='Local APFS, isolated PostgreSQL 18 data directory; fsync enabled' \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-stall-correlation-1000
```
