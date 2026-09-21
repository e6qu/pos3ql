# Bounded full-roster checkpoint, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean implementation commit `11186fc03efd2147c671674e711281d536c97872`.
The manifest records the release binary SHA-256, host, toolchain, 2 GiB fixed
disk cache, eight-second minimum workload duration, 300-second query timeout,
and settling checkpoint before the interference window.

This run repeats the exact 10,000-row profile that previously exposed a
27.50-second full-roster row rewrite. A dirty filled generation list now merges
an adjacent pair through restartable checkpoint beats before appending its new
delta. Finished merges have one fixed startup slot per table, so several filled
tables can be prepared for one manifest publish without sending another table
through the old rewrite path. The profile records schedule and write beats
separately.

No `row_sst_full` event occurred. The replacement merge used 64 schedule beats
and 207 write beats. Schedule beats read 4,712 blocks over 16.10 seconds in
total; their largest event took 447.94 ms and read 121 blocks. Write beats
wrote 844 blocks over 4.24 seconds; their largest event took 61.66 ms and wrote
eight blocks. The five following row deltas wrote 75 blocks over 0.36 seconds.
The prior single rewrite read 6,600 blocks, wrote 1,337, and occupied one
27.50-second dispatch.

All 400 minimum foreground operations and three explicit checkpoints completed
without error. Checkpoint-pressure throughput rose from 5.09 to 11.25
operations per second, p99 fell from 5,600.19 to 4,030.29 ms, and maximum
latency fell from 30,271.24 to 6,242.31 ms. The workload shapes outside the
target also differed: this run recorded five value-index events over 15.15
seconds, while the prior run recorded fourteen over 41.03 seconds. These two
shared-host runs are useful diagnostic evidence rather than a controlled
production ratio. The remaining largest individual checkpoint event was a
3.91-second value-index publication.

Actual PostgreSQL 18.6 completed 50,583 operations during the matched
checkpoint-pressure workload at 6,322.07 operations per second, 3.90 ms p99,
and 79.31 ms maximum latency. PostgreSQL ran unmodified with `fsync`,
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
tools/run-performance.sh checkpoint ./performance-results/checkpoint-full-roster-bounded-10000
```
