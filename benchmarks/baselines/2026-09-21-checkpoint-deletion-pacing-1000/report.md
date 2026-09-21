# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `6638cc2a08ea87ff1f7fd96bc50f69609faabd6d`; binary `168a1da85b80d5ec…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; storage: Local APFS, isolated PostgreSQL 18 data directory; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 309.02 | 13.23 | 41.08 | 76.10 | 133.02 | 290.80 | 1519 | 0 | 0.679 | 0 |
| mixed-checkpoint-interference | 26.31 | 48.06 | 602.32 | 1876.28 | 1894.58 | 306.47 | 200 | 0 | 8.700 | 0 |
| point-concurrency-1 | 19.50 | 51.18 | 56.41 | 64.39 | 64.39 | 290.50 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 17787.67 | 0.21 | 0.39 | 0.53 | 4.16 | — | 71163 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 18795.93 | 0.20 | 0.37 | 0.50 | 5.31 | — | 75195 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 8720.61 | 0.10 | 0.17 | 0.80 | 0.80 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 8 | 251.10 | 0 | 0 | 42 |
| gc_delete | 33 | 1440.52 | 0 | 0 | 468 |
| gc_keep | 8 | 2.61 | 0 | 0 | 0 |
| gc_list | 8 | 65.63 | 0 | 0 | 0 |
| legacy_gc | 8 | 59.99 | 0 | 0 | 0 |
| local_cleanup | 8 | 0.58 | 0 | 0 | 0 |
| manifest | 8 | 29.48 | 0 | 0 | 0 |
| row_sst | 11 | 1816.95 | 29 | 407 | 0 |
| value_indexes | 11 | 3129.27 | 0 | 534 | 0 |

Full profile window: 1086 object PUT, 516 object DELETE, 32 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 26 | 25 | 1 | — | 0.77 | 104.78 | 104.94 | 104.94 |
| commit_prune | 36 | 17 | 19 | 835.46 | 71.94 | 1876.37 | 1894.58 | 1894.58 |
| gc_delete | 78 | 59 | 19 | 4931.28 | 59.44 | 1876.19 | 1894.58 | 1894.58 |
| gc_keep | 35 | 28 | 7 | 7.66 | 59.51 | 1876.37 | 1894.58 | 1894.58 |
| gc_list | 25 | 18 | 7 | 199.82 | 59.87 | 1876.37 | 1894.58 | 1894.58 |
| legacy_gc | 36 | 23 | 13 | 179.31 | 71.94 | 1876.37 | 1894.58 | 1894.58 |
| local_cleanup | 24 | 11 | 13 | 1.98 | 28.62 | 1876.37 | 1894.58 | 1894.58 |
| manifest | 36 | 23 | 13 | 89.19 | 28.43 | 1876.37 | 1894.58 | 1894.58 |
| row_sst | 60 | 45 | 15 | 6553.09 | 446.03 | 1876.19 | 1894.58 | 1894.58 |
| value_indexes | 36 | 25 | 11 | 11468.98 | 505.69 | 1876.37 | 1894.58 | 1894.58 |

Profiled phases intersected 24266.76 ms of 30308.92 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.341 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 24.65x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
