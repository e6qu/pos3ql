# Paced checkpoint deletion, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
feature-enabled run from clean commit
`b631d2e84cfc922bc14bd69910f6fff2933135c3`. The manifest records the
release binary SHA-256, host, toolchain, cache, four-second minimum workload
duration, 120-second query timeout, and settling checkpoint before the
interference window.

Automatic checkpoint maintenance now schedules covered commit-object pruning
after manifest publication and deletes at most
`checkpoint_delete_objects_per_beat` objects from one namespace in each
dispatch beat. Its default is 16; the independent 4,096-object staging batch
retains a scanned candidate set across beats. A value-one simulator regression covers
commit batches and descriptors, legacy SSTs, block orphans, retries, complete
drain, and object-cold recovery. Explicit `CHECKPOINT` retains its synchronous
contract and drains all pending maintenance before returning.

The configured deletion limit is visible in the phase trace. The prior
[final-slice run](../2026-09-21-checkpoint-final-slice-1000/README.md) placed as
many as 244 commit-object DELETEs in one 664.63 ms phase event and 131 block
DELETEs in one 340.33 ms event. This run placed no more than 16 DELETEs in an
event: the maximum commit-prune event was 100.13 ms and the maximum block-delete
event was 89.75 ms. Total work differs between the runs, so these event maxima
qualify the pacing boundary rather than an end-to-end speedup. The three
explicit checkpoint commands drain adjacent batches and therefore still block
their command handler until cleanup completes.

| pos3ql phase | Events | Phase time | Maximum event | Object DELETE |
|---|---:|---:|---:|---:|
| Value-index rebuild | 3 | 1,051 ms | 546 ms | 0 |
| Row SST publication | 3 | 517 ms | 208 ms | 0 |
| Commit-batch pruning | 17 | 1,017 ms | 100 ms | 232 |
| Block garbage deletion | 15 | 670 ms | 90 ms | 207 |
| Manifest publication | 4 | 14 ms | 4 ms | 0 |

The full window recorded 714 object PUTs, 448 DELETEs, 54 LISTs, and 278 ranged
GETs. All 623 pos3ql foreground operations completed in 4.01 seconds. Its p99
was 82.92 ms without explicit checkpoints and 640.92 ms with them. Actual
PostgreSQL 18.6 completed 70,652 operations and three explicit checkpoints in
the interference workload; its p99 was 0.72 ms without explicit checkpoints and
0.58 ms with them. These shared-host timings are exploratory. PostgreSQL used
an isolated local APFS data directory with `fsync`, `full_page_writes`, and
`synchronous_commit` on. pos3ql used a 1 GiB fixed disk cache and an
instrumented S3-compatible fixture backed by temporary local storage with 2 ms
injected request latency.

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
