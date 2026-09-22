# Retained row-merge cursors and shared-container reads, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean implementation commit `1678374994de99353ffdb4657f6f06267afe5bb1`.
The release binary SHA-256 is
`137332d220317b1b9a965782edc65e98f52a81664859fe041fa31ee78dcfa069`.
The manifest records the host, toolchain, 2 GiB fixed disk cache, two-millisecond
injected object latency, eight-second minimum workload duration, 300-second
query timeout, and settling checkpoint before the interference window.

The merge writer now retains each source cursor and its decoded physical group
across paced beats. Sparse-key lookup advances over source blocks that the
merge schedule pruned instead of reading them sequentially. Full-row PAX
materialization fetches a shared packed container once, then verifies and
decodes each logical column frame from fixed startup-accounted scratch.
Selective query paths still fetch only demanded columns.

Compared with the preceding [packed-row profile](../2026-09-22-checkpoint-row-format-10000/README.md),
the clean run recorded:

| `row_merge_write` measure | Preceding run | This run |
|---|---:|---:|
| events | 440 | 228 |
| block GET | 5,782 | 0 |
| block PUT | 906 | 922 |
| summed phase time | 21,306.20 ms | 7,079.40 ms |
| largest event GET | 17 | 0 |
| largest event span | 1,208.45 ms | 334.94 ms |

The zero durable-tier GET count is a warm-cache result: complete container
reads reuse the fixed disk cache already populated by the workload, while the
former per-column ranged reads did not reuse a complete cached object. A
separate deterministic regression disables RAM and disk caches, forces a
12-column full roster through row merge, and checks both warm and empty-cache
recovery. It observes 19 provider GETs across five beats and at most six GETs
in one beat. Those request bounds, rather than shared-host timing or differing
event counts, are the controlled result.

The measured implementation reserved and charged 10,220,676 bytes of fixed
startup scratch for two retained source groups and their shared decoding
buffers. The profile's maximum resident set was 378.58 MiB. The final change
reuses the assembled-row buffer for transient PAX ranges, reducing that charge
to 5,765,578 bytes without changing object reads. Snapshot pruning, tombstones,
retry, and v2/v3/v4 row-format identity remain covered by the regression suite.

Row merge still wrote 922 immutable objects. Reusing unchanged PAX groups or
their references during merged-generation construction is the next measured
checkpoint target. The same run also recorded 1,669 GETs over 481
`value_index_schedule` events, which provides the next comparison after row
merge output traffic is bounded.

All 400 minimum foreground operations and three explicit checkpoints completed
without error. Checkpoint-pressure throughput was 24.10 operations per second,
p99 was 86.07 ms, and maximum latency was 14.60 seconds. Actual PostgreSQL 18.6
completed the matched workload at 1,857.52 operations per second, 12.89 ms p99,
and 205.96 ms maximum latency. PostgreSQL ran unmodified with `fsync`,
`full_page_writes`, and `synchronous_commit` enabled on its recorded
Docker-managed local volume. pos3ql used an instrumented S3-compatible fixture
backed by temporary local storage. The persistence tiers and operation counts
differ, so object-request measurements apply only to pos3ql and these timings
do not establish production ratios.

Reproduce with Docker available for the actual PostgreSQL 18 comparison:

```sh
POS3QL_BENCH_ROWS=10000 POS3QL_BENCH_TABLE_CAPACITY=16384 \
POS3QL_BENCH_OPERATIONS=100 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=2048 \
POS3QL_BENCH_TIMEOUT_SECONDS=300 POS3QL_BENCH_CHECKPOINT_SECONDS=8 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-row-merge-containers-10000
```
