# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `b631d2e84cfc922bc14bd69910f6fff2933135c3`; binary `7e8ffb6ed8b97bca…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; storage: Local APFS, isolated PostgreSQL 18 data directory; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 315.78 | 12.58 | 43.29 | 82.92 | 186.02 | 290.70 | 1647 | 0 | 0.696 | 0 |
| mixed-checkpoint-interference | 155.26 | 13.35 | 30.44 | 640.92 | 1034.87 | 303.92 | 623 | 0 | 1.490 | 0 |
| point-concurrency-1 | 20.06 | 47.43 | 59.18 | 94.76 | 94.76 | 290.38 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 16970.55 | 0.20 | 0.40 | 0.72 | 8.05 | — | 67891 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 17656.74 | 0.21 | 0.41 | 0.58 | 2.77 | — | 70652 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 8865.18 | 0.08 | 0.16 | 0.94 | 0.94 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 17 | 1016.60 | 0 | 0 | 232 |
| gc_delete | 15 | 670.20 | 0 | 0 | 207 |
| gc_keep | 15 | 1.62 | 0 | 0 | 0 |
| gc_list | 15 | 121.57 | 0 | 0 | 0 |
| legacy_gc | 4 | 27.19 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.27 | 0 | 0 | 0 |
| manifest | 4 | 14.17 | 0 | 0 | 0 |
| row_sst | 3 | 517.48 | 19 | 111 | 0 |
| value_indexes | 3 | 1050.65 | 0 | 214 | 0 |

Full profile window: 714 object PUT, 448 object DELETE, 54 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 611 | 487 | 124 | — | 13.30 | 28.38 | 35.44 | 56.43 |
| commit_prune | 12 | 10 | 2 | 256.32 | 640.92 | 1034.87 | 1034.87 | 1034.87 |
| gc_delete | 12 | 10 | 2 | 896.31 | 640.92 | 1034.87 | 1034.87 | 1034.87 |
| gc_keep | 12 | 10 | 2 | 4.14 | 640.92 | 1034.87 | 1034.87 | 1034.87 |
| gc_list | 12 | 10 | 2 | 205.22 | 640.92 | 1034.87 | 1034.87 | 1034.87 |
| legacy_gc | 12 | 10 | 2 | 79.39 | 640.92 | 1034.87 | 1034.87 | 1034.87 |
| local_cleanup | 12 | 10 | 2 | 0.73 | 640.92 | 1034.87 | 1034.87 | 1034.87 |
| manifest | 12 | 10 | 2 | 44.06 | 640.92 | 1034.87 | 1034.87 | 1034.87 |
| row_sst | 8 | 6 | 2 | 1433.39 | 636.90 | 641.42 | 641.42 | 641.42 |
| value_indexes | 8 | 6 | 2 | 2018.46 | 636.90 | 641.42 | 641.42 | 641.42 |

Profiled phases intersected 4938.01 ms of 16044.57 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.319 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 7.73x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
