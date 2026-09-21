# Paced checkpoint deletion, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
feature-enabled run from clean commit
`6638cc2a08ea87ff1f7fd96bc50f69609faabd6d`. The manifest records the
release binary SHA-256, host, toolchain, cache, four-second minimum workload
duration, 120-second query timeout, and settling checkpoint before the
interference window.

Automatic checkpoint maintenance schedules covered commit-object pruning
after manifest publication and deletes at most
`checkpoint_delete_objects_per_beat` objects from one namespace in each
dispatch beat. Its default is 16. The independent 4,096-object staging batch
retains a scanned candidate set across beats. A value-one simulator regression
covers commit batches and descriptors, legacy SSTs, block orphans, retries,
complete drain, object-cold recovery, and the number of namespace scans.
Explicit `CHECKPOINT` retains its synchronous contract and drains all pending
maintenance before returning.

The corrected staging boundary is visible in both request and phase traces.
This run published eight manifests and made 32 LIST requests: exactly the two
commit passes, one legacy SST pass, and one block pass required per
publication. Its 33 block-delete events did not trigger another namespace
scan. This matches the final-slice run's 16 LIST requests for four
publications while spreading block deletion across many more phase events.

The configured deletion limit is also visible in the phase trace. The prior
[final-slice run](../2026-09-21-checkpoint-final-slice-1000/README.md) placed as
many as 244 commit-object DELETEs in one 664.63 ms phase event and 131 block
DELETEs in one 340.33 ms event. This run placed no more than 16 DELETEs in an
event: the maximum commit-prune event deleted 12 objects in 54.40 ms and the
maximum block-delete event deleted 16 objects in 52.46 ms. Total work and the
number of publications differ between the runs, so these event maxima qualify
the pacing boundary rather than an end-to-end speedup. The three explicit
checkpoint commands drain adjacent batches and therefore still block their
command handler until cleanup completes.

| pos3ql phase | Events | Phase time | Maximum event | Object DELETE |
|---|---:|---:|---:|---:|
| Value-index rebuild | 11 | 3,129 ms | 347 ms | 0 |
| Row SST publication | 11 | 1,817 ms | 206 ms | 0 |
| Commit-batch pruning | 8 | 251 ms | 54 ms | 42 |
| Block garbage deletion | 33 | 1,441 ms | 52 ms | 468 |
| Manifest publication | 8 | 29 ms | 4 ms | 0 |

The full window recorded 1,086 object PUTs, 516 DELETEs, 32 LISTs, and 288
ranged GETs. All 200 pos3ql foreground operations completed; its p99 was 76.10
ms without explicit checkpoints and 1,876.28 ms with them. The window included
eight publications and eleven row and value-index rebuilds, so it is evidence
for the deletion and scan boundaries rather than a stable throughput ratio.
Actual PostgreSQL 18.6 completed 75,195 operations and three explicit
checkpoints in the interference workload; its p99 was 0.53 ms without explicit
checkpoints and 0.50 ms with them. These shared-host timings are exploratory.
PostgreSQL used an isolated local APFS data directory with `fsync`,
`full_page_writes`, and `synchronous_commit` on. pos3ql used a 1 GiB fixed disk
cache and an instrumented S3-compatible fixture backed by temporary local
storage with 2 ms injected request latency.

Reproduce with an isolated PostgreSQL 18 server configured as recorded in
`postgresql-server.json`, then run:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 POS3QL_BENCH_CHECKPOINT_SECONDS=4 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 POS3QL_BENCH_POSTGRES_PORT="$PG_PORT" \
POS3QL_BENCH_POSTGRES_STORAGE='Local APFS, isolated PostgreSQL 18 data directory; fsync enabled' \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-deletion-pacing-1000
```
