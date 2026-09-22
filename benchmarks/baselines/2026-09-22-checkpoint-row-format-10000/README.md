# Packed row deltas and bounded row merge reads, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean implementation commit `931d83916651d874133e96d0f39a873fc7d1d33a`.
The manifest records the release binary SHA-256, host, toolchain, 2 GiB fixed
disk cache, eight-second minimum workload duration, 300-second query timeout,
and settling checkpoint before the interference window.

The v4 row SST format uses one packed-reference grammar for PAX full slices
and row-packed deltas. Row deltas compress canonical row groups and coalesce
their independently verified frames into immutable container objects. Full
slices retain PAX column groups. Readers keep v2 direct and v3 PAX support, so
one manifest may reference every supported generation during online
replacement.

Row compaction's schedule pass now reads keys, commit LSNs, and tombstones from
each PAX descriptor without materializing column extents. Schedule beats stop
after eight data groups. Write beats additionally stop after eight object
reads once the current row is complete; one indivisible wide PAX row can cross
that limit.

The exact profile recorded 37 `row_merge_schedule` beats over 642.90 ms. They
made 238 block GETs in total, with an eight-GET and 23.91 ms maximum. The prior
[paced source-and-sort run](../2026-09-22-checkpoint-value-index-schedule-10000/README.md)
recorded 102 schedule beats over 27.35 seconds, 8,268 GETs, and maxima of 121
GETs and 401.72 ms. Event counts and cache state changed, so aggregate timing
is diagnostic; the per-beat request boundary is the completed result.

Two `row_sst_delta` events each wrote four blocks and together took 49.91 ms.
The prior run's two delta events each wrote 27 blocks and together took 252.78
ms. A deterministic 128-row regression independently requires one packed row
container plus its filter, index, and roster, then verifies warm reads and
empty-cache recovery.

The profile also makes the next boundary explicit. Row merge writing made
5,782 block GETs across 440 beats while materializing selected PAX rows after
the metadata-only schedule. A beat made at most 17 GETs, the cost of one wide
row, and at most six PUTs. Retaining source row cursors and decoded groups
across write beats should remove repeated point-read setup without weakening
snapshot pruning, retry, fixed memory, or the per-beat request boundary. One
shared-host write event took 1.21 seconds despite only 17 GETs, so these timings
do not establish a production latency ratio.

All 400 minimum foreground operations and three explicit checkpoints completed
without error. Checkpoint-pressure throughput was 13.01 operations per second,
p99 was 89.55 ms, and maximum latency was 27.02 seconds. Actual PostgreSQL 18.6
completed the matched workload at 5,164.86 operations per second, 5.48 ms p99,
and 33.19 ms maximum latency. PostgreSQL ran unmodified with `fsync`,
`full_page_writes`, and `synchronous_commit` enabled on its recorded
Docker-managed local volume. pos3ql used an instrumented S3-compatible fixture
backed by temporary local storage with 2 ms injected request latency. The
persistence tiers and operation counts differ, so object-request measurements
apply only to pos3ql.

Reproduce with Docker available for the actual PostgreSQL 18 comparison:

```sh
POS3QL_BENCH_ROWS=10000 POS3QL_BENCH_TABLE_CAPACITY=16384 \
POS3QL_BENCH_OPERATIONS=100 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=2048 \
POS3QL_BENCH_TIMEOUT_SECONDS=300 POS3QL_BENCH_CHECKPOINT_SECONDS=8 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-row-format-10000
```
