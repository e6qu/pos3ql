# Incremental checkpoint row reslicing, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean commit `c0c46276a7b376af00b015e81e7252cb4239dc76`. The
manifest records the release binary SHA-256, host, toolchain, cache, four-second
minimum workload duration, 120-second query timeout, and settling checkpoint
before the interference window.

A table changed after an earlier slice in the same checkpoint sweep now keeps
that immutable slice and appends only row versions committed after its captured
LSN. A changed physical layout or a full spill-generation roster rebuilds from
the published base. Startup-sized LSN boundaries map warm heap rows to the SST
that contains them if later memory pressure evicts them; publication itself
keeps those reads warm.

The deterministic two-table regression writes 13 blocks for its first
128-row slice and 5 for a one-row reslice, then verifies deferred eviction and
empty-cache recovery across all three row generations. This profile also
exercised reslicing under the mixed workload. Two sweeps wrote a 39-block slice
followed by a 5-block reslice before publication. Another wrote a 137-block
slice followed by four 5-block reslices. In total, 12 row-SST events wrote 362
blocks across seven manifest events. These counts show that later slices carry
only their new versions; they do not form a controlled before-and-after timing
comparison because the run performed more automatic checkpoint work than the
preceding selective-index profile.

All 200 pos3ql foreground operations and three explicit checkpoints completed.
Its p99 was 69.56 ms without explicit checkpoints and 983.23 ms with them.
Actual PostgreSQL 18.6 completed 8,038 operations and three explicit
checkpoints in its matched interference workload, with p99 of 9.55 ms in the
baseline and 11.82 ms under checkpoint pressure. PostgreSQL ran unmodified in
the recorded Docker image with `fsync`, `full_page_writes`, and
`synchronous_commit` enabled on a Docker-managed local volume. pos3ql used a
1 GiB fixed disk cache and an instrumented S3-compatible fixture backed by
temporary local storage with 2 ms injected request latency. PostgreSQL's local
durable tier has no corresponding object-request metric.

Reproduce with Docker available for the actual PostgreSQL 18 comparison:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 POS3QL_BENCH_CHECKPOINT_SECONDS=4 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 \
POS3QL_BENCH_POSTGRES_STORAGE='Docker-managed local volume on host APFS; fsync enabled' \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-row-reslice-1000
```
