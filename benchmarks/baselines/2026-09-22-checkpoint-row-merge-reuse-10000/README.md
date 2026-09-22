# Immutable PAX group reuse during row merge, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean implementation commit `6caf6eff37b898e2af782f08c6e260574ae6a40e`.
The release binary SHA-256 is
`f5bd624b880c98fbf3a8eb3b3b4d9f04edf801ef301a2d954baf282fb2756924`.
The manifest records the host, toolchain, 2 GiB fixed disk cache, two-millisecond
injected object latency, eight-second minimum workload duration, 300-second
query timeout, and settling checkpoint before the interference window.

Row compaction now compares a complete decoded source PAX group with the
pruned merge schedule. When every key, source rank, and tombstone survives in
the same order, the merged SST points at the existing immutable descriptor and
column containers. The writer adds their physical identities to the new
roster before publication, so garbage collection may remove the source SST
without removing shared data. A group affected by duplicate replacement,
snapshot pruning, or head-tombstone removal is rebuilt through the ordinary
row writer. The checksum still covers reconstructed canonical rows, and the
path uses the existing fixed startup scratch.

Compared with the preceding [retained-cursor profile](../2026-09-22-checkpoint-row-merge-containers-10000/README.md),
the clean run recorded:

| `row_merge_write` measure | Preceding run | This run |
|---|---:|---:|
| events | 228 | 234 |
| block GET | 0 | 0 |
| block PUT | 922 | 90 |
| zero-PUT events | 0 | 212 |
| mean PUT per event | 4.04 | 0.38 |
| summed phase time | 7,079.40 ms | 1,065.04 ms |
| largest event PUT | 8 | 5 |
| largest event span | 334.94 ms | 39.24 ms |

Both runs used the same configured workload shape, but the duration floor let
this faster run complete 825 foreground operations versus 400 in the preceding
run. The phase event counts are close, while aggregate foreground timing and
garbage counts are not a controlled comparison. The object PUT reduction is
also covered by a deterministic cache-disabled regression: a merge containing
both a modified group and an unchanged group writes at most nine objects,
preserves the changed rows, and reads reused payload columns after garbage
collection and empty-cache recovery. Disabling group reuse in that fixture
writes 13 objects.

All 825 checkpoint-pressure operations and three explicit checkpoints
completed without error. Checkpoint-pressure throughput was 102.80 operations
per second, p99 was 62.48 ms, and maximum latency was 4.13 seconds. Actual
PostgreSQL 18.6 completed the matched workload at 1,774.10 operations per
second, 16.29 ms p99, and 546.28 ms maximum latency. PostgreSQL ran unmodified
with `fsync`, `full_page_writes`, and `synchronous_commit` enabled on its
recorded Docker-managed local volume. pos3ql used an instrumented S3-compatible
fixture backed by temporary local storage. The persistence tiers and operation
counts differ, so object-request measurements apply only to pos3ql and these
timings do not establish production ratios.

With row merge reduced to 90 PUTs and zero GETs, `value_index_schedule` is the
next checkpoint target. It made 910 GETs and 162 PUTs over 398 events in this
run. The next change should reduce that physical source and external-run work
without weakening its four-PUT, eight-GET pacing, fixed-memory sort, retry, or
publication guarantees.

Reproduce with Docker available for the actual PostgreSQL 18 comparison:

```sh
POS3QL_BENCH_ROWS=10000 POS3QL_BENCH_TABLE_CAPACITY=16384 \
POS3QL_BENCH_OPERATIONS=100 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=2048 \
POS3QL_BENCH_TIMEOUT_SECONDS=300 POS3QL_BENCH_CHECKPOINT_SECONDS=8 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-row-merge-reuse-10000
```
