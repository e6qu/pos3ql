# Paced value-index checkpoint writer, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean implementation commit `f1dd2184dd912b6f6eeb6f06f16a1538da4a4ca8`.
The manifest records the release binary SHA-256, host, toolchain, 2 GiB fixed
disk cache, eight-second minimum workload duration, 300-second query timeout,
and settling checkpoint before the interference window.

Each affected value-index binding now collects and externally sorts its source
once, then retains that immutable source while its output writer advances over
later dispatch beats. A writer beat processes at most 1,024 entries and stops
after four block PUTs or eight block GETs. The sorter, run reader, entry buffer,
writer, and job state are charged at startup. A binding's exact dirty LSN
invalidates an older job or staged generation after an interleaved commit,
including the next-LSN marker used by `REINDEX`. Table identity keeps equal
binding ordinals on different relations independent, and a dirty binding is
scheduled even when its row slice is already clean. A failed output beat resets
the writer and replays the retained sorted source; content-addressed writes make
that retry idempotent.

The profile recorded 74 `value_index_write` beats. Together they took 153.38
ms and wrote 28 blocks. The largest beat took 15.57 ms and wrote four blocks;
no beat crossed the four-PUT or eight-GET boundary. The prior 10,000-row run
recorded five combined value-index events totaling 15.15 seconds, with one
3.91-second event and as many as 79 PUTs and 316 GETs in one dispatch. The new
profile separates collection and sorting into `value_index_schedule`: 24 such
events took 21.26 seconds, wrote 486 temporary sort blocks, and reached 1.41
seconds, 22 PUTs, and 100 GETs in one event. Event counts and the amount of
row-merge work changed between runs, so the aggregate totals are diagnostic
rather than a controlled timing ratio. The remaining value-index dispatch
target is now source collection and external run generation.

All 400 minimum foreground operations and three explicit checkpoints completed
without error. Checkpoint-pressure throughput was 6.93 operations per second,
p99 was 8,087.58 ms, and maximum latency was 13,537.33 ms. This run performed
99 row-merge schedule beats and 269 row-merge write beats, compared with 64 and
207 in the prior profile, and published four manifests rather than eight.
Those workload-shape differences prevent attributing the worse end-to-end
latency to writer pacing alone.

Actual PostgreSQL 18.6 completed 18,703 operations during the matched
checkpoint-pressure workload at 2,337.43 operations per second, 10.32 ms p99,
and 31.49 ms maximum latency. PostgreSQL ran unmodified with `fsync`,
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
tools/run-performance.sh checkpoint ./performance-results/checkpoint-value-index-pacing-10000
```
