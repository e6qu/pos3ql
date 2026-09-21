# Checkpoint reslice scale, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean commit `d3f8087a0b44988ef0c88b56780040da4e817ca3`. The
manifest records the release binary SHA-256, host, toolchain, 2 GiB disk cache,
eight-second minimum workload duration, 300-second query timeout, and settling
checkpoint before the interference window.

The scale run exposed two read-amplification paths before it could complete.
Checkpoint reslices rebuilt staged value-index bindings whose dependent values
had not changed after the earlier slice. Staged generations now retain the LSN
of the change they cover, so a compatible reslice carries unchanged handles
forward. Cold `ANALYZE`, `CREATE INDEX` validation, and resident value-cache
population also reopened object-resident rows or requested every packed column
as a separate range. They now stream the merged row cursor. Full-column scans
fetch and verify one packed container at a time; value-cache population requests
the union of binding dependencies and coalesces only when at least half the
physical columns are needed. Selective query scans retain their ranged-column
path.

The profile answers the row-image versus value-index question in two parts.
Across compatible reslices, affected value indexes dominated steady publication:
14 events took 41.03 seconds and wrote 1,017 blocks, while 13 row deltas took
0.92 seconds and wrote 197 blocks. Most value-index events read no durable
blocks, confirming that unchanged staged bindings were retained. One full-roster
row compaction was the largest individual event: it took 27.50 seconds, read
6,600 blocks, and wrote 1,337. Repeated value-index publication therefore
dominates ordinary resliced sweeps in aggregate, while a complete row rewrite
dominates the worst single pause at this scale.

All 100 setup point reads and 400 minimum foreground operations completed, as
did three explicit checkpoints. pos3ql recorded a 122.68 ms p99 without
explicit checkpoint pressure and 5,600.19 ms with it; the full row compaction
produced a 30.27 second maximum. Actual PostgreSQL 18.6 completed 49,664
baseline and 44,359 checkpoint-pressure operations, with p99 values of 3.83 ms
and 4.63 ms. PostgreSQL ran unmodified with `fsync`, `full_page_writes`, and
`synchronous_commit` enabled on its recorded Docker-managed local volume.
pos3ql used an instrumented S3-compatible fixture backed by temporary local
storage with 2 ms injected request latency. The persistence tiers and operation
counts differ, so these shared-host measurements are exploratory rather than a
production ratio.

Reproduce with Docker available for the actual PostgreSQL 18 comparison:

```sh
POS3QL_BENCH_ROWS=10000 POS3QL_BENCH_TABLE_CAPACITY=16384 \
POS3QL_BENCH_OPERATIONS=100 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=2048 \
POS3QL_BENCH_TIMEOUT_SECONDS=300 POS3QL_BENCH_CHECKPOINT_SECONDS=8 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-reslice-scale-10000
```
