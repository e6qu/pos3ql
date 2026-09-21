# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `979cd537e458e3416683d5f1a62dde9de1628a72`; binary `dcdca98033e63acc…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

PostgreSQL baseline: version `180006`; storage: Local APFS, isolated PostgreSQL 18 data directory; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 312.03 | 1.69 | 52.35 | 76.77 | 97.95 | 287.56 | 1689 | 0 | 0.666 | 0 |
| mixed-checkpoint-interference | 15.81 | 46.36 | 945.93 | 2548.02 | 2551.72 | 296.73 | 200 | 0 | 14.350 | 0 |
| point-concurrency-1 | 18.54 | 54.01 | 57.93 | 65.60 | 65.60 | 287.27 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 18379.68 | 0.20 | 0.37 | 0.52 | 14.54 | — | 73532 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 18324.90 | 0.20 | 0.39 | 0.54 | 10.92 | — | 73310 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 8553.89 | 0.10 | 0.16 | 0.74 | 0.74 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 9 | 1461.45 | 0 | 0 | 418 |
| gc_delete | 9 | 2462.65 | 0 | 0 | 768 |
| gc_keep | 9 | 6.77 | 0 | 0 | 0 |
| gc_list | 9 | 79.23 | 0 | 0 | 0 |
| legacy_gc | 9 | 68.23 | 0 | 0 | 0 |
| local_cleanup | 9 | 0.81 | 0 | 0 | 0 |
| manifest | 9 | 32.10 | 0 | 0 | 0 |
| row_sst | 14 | 3306.91 | 27 | 718 | 0 |
| value_indexes | 14 | 4411.30 | 0 | 754 | 0 |

Full profile window: 1621 object PUT, 1186 object DELETE, 36 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 31 | 26 | 5 | — | 0.51 | 42.17 | 42.18 | 42.18 |
| commit_prune | 32 | 23 | 9 | 5789.76 | 63.81 | 2551.66 | 2551.72 | 2551.72 |
| gc_delete | 28 | 19 | 9 | 6738.79 | 389.28 | 2551.66 | 2551.72 | 2551.72 |
| gc_keep | 47 | 28 | 19 | 10.88 | 164.29 | 2548.02 | 2551.72 | 2551.72 |
| gc_list | 28 | 19 | 9 | 242.96 | 389.28 | 2551.66 | 2551.72 | 2551.72 |
| legacy_gc | 51 | 32 | 19 | 235.87 | 48.19 | 2548.02 | 2551.72 | 2551.72 |
| local_cleanup | 32 | 23 | 9 | 3.18 | 63.81 | 2551.66 | 2551.72 | 2551.72 |
| manifest | 52 | 43 | 9 | 113.40 | 71.29 | 2548.02 | 2551.72 | 2551.72 |
| row_sst | 98 | 79 | 19 | 13227.34 | 438.90 | 989.97 | 2551.72 | 2551.72 |
| value_indexes | 56 | 41 | 15 | 17645.20 | 502.51 | 2548.02 | 2551.72 | 2551.72 |

Profiled phases intersected 44007.38 ms of 50579.34 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.351 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 33.19x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
