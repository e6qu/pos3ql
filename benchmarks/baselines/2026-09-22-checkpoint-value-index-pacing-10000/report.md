# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `f1dd2184dd912b6f6eeb6f06f16a1538da4a4ca8`; binary `973f5458e364c9ad…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 9.01 | 65.72 | 965.27 | 1268.04 | 1276.86 | 93.12 | 516 | 0 | 3.567 | 0 |
| mixed-checkpoint-interference | 6.93 | 347.73 | 919.43 | 8087.58 | 13537.33 | 93.38 | 5545 | 0 | 29.885 | 0 |
| point-concurrency-1 | 6.94 | 71.71 | 450.98 | 494.70 | 495.91 | 91.56 | 1602 | 0 | 40.320 | 0 |
| postgresql18-mixed-baseline | 2153.06 | 1.15 | 6.05 | 10.09 | 21.67 | — | 17233 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 2337.43 | 0.91 | 5.82 | 10.32 | 31.49 | — | 18703 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 2609.65 | 0.31 | 0.64 | 1.19 | 1.35 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 7 | 401.38 | 0 | 0 | 52 |
| gc_delete | 45 | 2098.88 | 0 | 0 | 674 |
| gc_keep | 4 | 176.97 | 58 | 0 | 0 |
| gc_list | 4 | 434.74 | 0 | 0 | 0 |
| legacy_gc | 4 | 122.27 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.27 | 0 | 0 | 0 |
| manifest | 4 | 13.87 | 0 | 0 | 0 |
| row_merge_schedule | 99 | 27833.09 | 8611 | 0 | 0 |
| row_merge_write | 269 | 4995.58 | 0 | 1087 | 0 |
| row_sst_delta | 2 | 161.08 | 0 | 30 | 0 |
| value_index_schedule | 24 | 21257.55 | 102 | 486 | 0 |
| value_index_write | 74 | 153.38 | 0 | 28 | 0 |

Full profile window: 1713 object PUT, 736 object DELETE, 25 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 20 | 20 | 0 | — | 43.75 | 49.15 | 51.60 | 51.60 |
| commit_prune | 12 | 12 | 0 | 771.31 | 8087.16 | 13537.33 | 13537.33 | 13537.33 |
| gc_delete | 12 | 12 | 0 | 6403.57 | 8087.16 | 13537.33 | 13537.33 | 13537.33 |
| gc_keep | 12 | 12 | 0 | 15.13 | 8087.16 | 13537.33 | 13537.33 | 13537.33 |
| gc_list | 12 | 12 | 0 | 1335.58 | 8087.16 | 13537.33 | 13537.33 | 13537.33 |
| legacy_gc | 12 | 12 | 0 | 363.68 | 8087.16 | 13537.33 | 13537.33 | 13537.33 |
| local_cleanup | 12 | 12 | 0 | 0.77 | 8087.16 | 13537.33 | 13537.33 | 13537.33 |
| manifest | 12 | 12 | 0 | 40.78 | 8087.16 | 13537.33 | 13537.33 | 13537.33 |
| row_merge_schedule | 246 | 186 | 60 | 108718.22 | 364.77 | 1417.14 | 13537.15 | 13537.33 |
| row_merge_write | 108 | 88 | 20 | 17607.32 | 64.68 | 8087.19 | 13537.19 | 13537.33 |
| row_sst_delta | 4 | 4 | 0 | 377.97 | 8087.16 | 8087.58 | 8087.58 | 8087.58 |
| value_index_schedule | 163 | 141 | 22 | 78076.30 | 855.98 | 920.11 | 8087.19 | 8087.58 |
| value_index_write | 85 | 73 | 12 | 284.80 | 63.38 | 382.20 | 8087.58 | 8087.58 |

Profiled phases intersected 213995.42 ms of 230770.03 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.389 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 6.38x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
