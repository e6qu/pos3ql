# Final-slice checkpoint publication, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
feature-enabled run from clean commit
`107f97f4f5e200a3ac3d5bf0ece5be91c9962a51`. The manifest records the
release binary SHA-256, host, toolchain, cache, four-second minimum workload
duration, and 120-second query timeout.

This run repeats the settings from the clean
[foreground-correlation baseline](../2026-09-21-checkpoint-stall-correlation-1000/README.md).
The change publishes a manifest in the same beat that writes the final stale
table slice. Earlier code yielded after that slice, so a foreground write
could invalidate it before the next beat and force another immutable row SST
and value-index generation.

| pos3ql measure | Before | Final-slice publish | Change |
|---|---:|---:|---:|
| Manifest publications | 9 | 9 | 0% |
| Row SST rebuilds | 14 | 9 | -36% |
| Value-index rebuilds | 14 | 9 | -36% |
| Row SST block PUTs | 718 | 465 | -35% |
| Value-index block PUTs | 754 | 505 | -33% |
| All object PUTs | 1,621 | 1,047 | -35% |
| Summed foreground phase intersection | 44,007 ms | 28,428 ms | -35% |

Both clean runs completed all 200 pos3ql checkpoint-overlap operations and
three explicit checkpoints. The new run completed them in 8.47 seconds rather
than 12.65 seconds; p99 was 2,387.70 ms rather than 2,548.02 ms. These timing
differences are exploratory on an unreserved shared host. The request counts
and equal publication count directly show that the five invalidated
generations were removed without skipping a durable manifest.

Actual PostgreSQL 18.6 completed 74,276 operations and three explicit
checkpoints in the new four-second comparison window. Its p99 was 0.37 ms
without explicit checkpoints and 0.76 ms with them. PostgreSQL used an
isolated local APFS data directory with `fsync`, `full_page_writes`, and
`synchronous_commit` on; pos3ql used a 1 GiB fixed disk cache and an
instrumented S3-compatible fixture backed by temporary local storage with 2
ms injected request latency. The systems use distinct persistence tiers.

Reproduce with an isolated PostgreSQL 18 server configured as recorded in
`postgresql-server.json`, then run:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 POS3QL_BENCH_CHECKPOINT_SECONDS=4 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 POS3QL_BENCH_POSTGRES_PORT="$PG_PORT" \
POS3QL_BENCH_POSTGRES_STORAGE='Local APFS, isolated PostgreSQL 18 data directory; fsync enabled' \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-final-slice-1000
```
