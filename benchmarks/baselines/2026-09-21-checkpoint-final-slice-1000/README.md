# Final-slice checkpoint publication, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
feature-enabled run from clean commit
`8fe85e06cad590b9a3cac2392801837270f538fa`. The manifest records the
release binary SHA-256, host, toolchain, cache, four-second minimum workload
duration, 120-second query timeout, and the settling checkpoint before the
interference window.

The change publishes a manifest in the same beat that writes the final stale
table slice when no merge beat is due. Earlier code always yielded after
that slice, so a foreground write could invalidate it before the next beat and
force another immutable row SST and value-index generation. When compaction is
pending, the yield remains so the alternating bounded merge beat cannot be
starved. A two-table regression proves the first slice remains paced and the
final slice publishes without a dispatch gap; the existing long-history
regression proves merge progress remains within fixed scratch.

The harness now completes one unmeasured checkpoint on each engine after its
baseline workload. This drains baseline publication and cleanup before the
profile offsets and request counters are captured. The measured window then
runs three explicit checkpoints. pos3ql produced three row and value-index
generations for those checkpoints. A fourth metadata-only manifest was written
during server shutdown, which is inside the full profile and object-request
window.

| pos3ql phase | Events | Phase time | Summed foreground intersection | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| Value-index rebuild | 3 | 1,035 ms | 1,907 ms | 224 | 0 |
| Row SST publication | 3 | 506 ms | 1,386 ms | 117 | 0 |
| Commit-batch pruning | 4 | 727 ms | 251 ms | 0 | 254 |
| Block garbage deletion | 4 | 564 ms | 894 ms | 0 | 218 |
| Manifest publication | 4 | 12 ms | 38 ms | 0 | 0 |

The full window recorded 765 object PUTs, 475 DELETEs, 16 LISTs, and 296
ranged GETs. Commit pruning and block garbage deletion account for 472
DELETEs; three requests fall outside those attributed delete counters. The
phase counters account for 341 block PUTs. The remaining PUTs include commit
publication and manifest writes outside the block-writer phases.

All 725 pos3ql foreground operations completed in 4.03 seconds. Its p99 was
75.12 ms without explicit checkpoints and 624.60 ms in the checkpoint-overlap
workload. Actual PostgreSQL 18.6 completed 73,534 operations and three explicit
checkpoints; its p99 was 0.61 ms and 0.59 ms. These shared-host timings are
exploratory. PostgreSQL used an isolated local APFS data directory with
`fsync`, `full_page_writes`, and `synchronous_commit` on. pos3ql used a 1 GiB
fixed disk cache and an instrumented S3-compatible fixture backed by temporary
local storage with 2 ms injected request latency.

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
