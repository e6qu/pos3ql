# Checkpoint phase profile, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
feature-enabled run from clean commit
`403e998d80a49b906d18b3de42050c3f18cb0dfb`. The manifest records the
release binary SHA-256, host, toolchain, cache, four-second minimum workload
duration, and 120-second query timeout. The compile-time profile feature
writes fixed-size lines to stderr without allocating after startup.

The mixed workload had 1,000 setup rows, four clients, and three explicit
checkpoints. All 388 pos3ql checkpoint-overlap operations and 76,794
PostgreSQL 18 checkpoint-overlap operations completed without errors. Their
operation counts differ because each workload ran for at least four seconds.
The pos3ql profile starts with its mixed workload and continues through server
stop, including cleanup after the timed queries end. Its phase spans therefore
cannot be summed into query latency or used as a direct cross-system ratio.

| pos3ql phase | Events | Elapsed | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|
| Commit-batch pruning | 3 | 1,590 ms | 0 | 550 |
| Value-index rebuild | 3 | 1,519 ms | 311 | 0 |
| Block garbage deletion | 3 | 820 ms | 0 | 296 |
| Row SST publication | 3 | 543 ms | 117 | 0 |

The phase counters account for all 846 DELETE requests in the profile's
object-store window. They account for 428 of 614 PUT requests; the remainder
includes commit publication and manifest writes outside the block writer
phases. The fixture also recorded 12 LIST requests and 19 ranged GETs in
that full window. These counts distinguish cleanup from publication without
claiming that every span delayed a foreground query.

The host was an unreserved Apple Silicon macOS 15.6.1 machine with 12 logical
CPUs and 24 GiB RAM. pos3ql used a 1 GiB fixed disk cache and an instrumented
S3-compatible fixture backed by temporary local storage with 2 ms injected
request latency. Actual PostgreSQL 18.6 used an isolated local APFS data
directory with `fsync`, `full_page_writes`, and `synchronous_commit` on. Its
local persistence has no matching object-request metric. Timing is
exploratory. The phase results identify commit pruning, value-index rebuild,
and block deletion as the next paths to examine for foreground interference.

Reproduce with an isolated PostgreSQL 18 server configured as recorded in
`postgresql-server.json`, then run:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 POS3QL_BENCH_CHECKPOINT_SECONDS=4 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 POS3QL_BENCH_POSTGRES_PORT="$PG_PORT" \
POS3QL_BENCH_POSTGRES_STORAGE='Local APFS, isolated PostgreSQL 18 data directory; fsync enabled' \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-phase-1000
```
