# Paced value-index source and sort, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean implementation commit `e3b5b80cfd4150380486803a6e92e898eb3d4300`.
The manifest records the release binary SHA-256, host, toolchain, 2 GiB fixed
disk cache, eight-second minimum workload duration, 300-second query timeout,
and settling checkpoint before the interference window.

Value-index source collection now retains its resident-map and merged-spill
positions across checkpoint beats. Opening a long spill-generation list and
walking rows both yield on the object-read counter. A shared scan context keeps
owned cursor heads when another walk displaces its buffers and reloads only the
member needed to materialize or advance the current row. PAX sources read only
the key, predicate, expression, and included-column dependencies of the active
binding. External run generation uses startup-allocated chunk metadata and
buffers, writes provider-neutral temporary SSTs, and merges them through
restartable binary carry state. One beat processes at most 1,024 source rows
and stops after eight object GETs or four block PUTs.

Two faults found while qualifying multiple external runs are covered directly.
A deferred source row now resides in the idle chunk buffer while a carry merge
reuses the entry buffer. A detached immutable-run cursor that resumes exactly
at a data-block boundary reloads that block, recognizes its saved end offset,
and advances to the next block. The wider checkpoint regression crosses three
sort chunks and verifies publication plus object-cold recovery. The PAX
regression resumes within a multi-block physical source and measures every
active value-index beat.

The profile recorded 461 `value_index_schedule` beats. Together they took
6.70 seconds, read 1,542 blocks, and wrote 162 temporary run blocks. The
largest beat took 36.42 ms; no event exceeded eight GETs or four PUTs. The
preceding [writer-pacing profile](../2026-09-22-checkpoint-value-index-pacing-10000/README.md)
recorded 24 schedule events over 21.26 seconds, with a 1.41-second maximum and
as many as 100 GETs and 22 PUTs in one event. Its schedule phase wrote 486
blocks. Counts and cache state changed between shared-host runs, so the totals
are diagnostic; the per-event I/O boundary is the completed result.

All 400 minimum foreground operations and three explicit checkpoints completed
without error. Checkpoint-pressure throughput was 9.60 operations per second,
p99 was 12,362.08 ms, and maximum latency was 13,758.27 ms. The next largest
profiled dispatch was row-merge source scheduling: 102 events took 27.35
seconds, and its largest event took 401.72 ms and read 121 blocks. Two row SST
delta events each wrote 27 blocks and reached 127.91 ms. Durable row and
compaction representation is therefore the next checkpoint design boundary;
format version and migration rules must precede a persisted-format change.

Actual PostgreSQL 18.6 completed 51,232 operations during the matched
checkpoint-pressure workload at 6,403.68 operations per second, 4.00 ms p99,
and 107.28 ms maximum latency. PostgreSQL ran unmodified with `fsync`,
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
tools/run-performance.sh checkpoint ./performance-results/checkpoint-value-index-schedule-10000
```
