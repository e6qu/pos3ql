# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `1678374994de99353ffdb4657f6f06267afe5bb1`; binary `137332d220317b1b…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 136.89 | 26.59 | 69.53 | 103.35 | 135.23 | 378.27 | 1415 | 0 | 0.912 | 0 |
| mixed-checkpoint-interference | 24.10 | 10.73 | 62.84 | 86.07 | 14595.28 | 378.58 | 707 | 0 | 7.120 | 0 |
| point-concurrency-1 | 39.26 | 21.91 | 40.43 | 48.09 | 50.19 | 377.92 | 100 | 0 | 4.530 | 0 |
| postgresql18-mixed-baseline | 1653.92 | 1.13 | 8.00 | 23.43 | 118.74 | — | 13235 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 1857.52 | 1.19 | 6.65 | 12.89 | 205.96 | — | 14869 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 1521.32 | 0.50 | 2.03 | 3.64 | 3.80 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 7 | 459.14 | 0 | 0 | 82 |
| gc_delete | 39 | 2222.14 | 0 | 0 | 608 |
| gc_keep | 3 | 3.55 | 0 | 0 | 0 |
| gc_list | 3 | 351.51 | 0 | 0 | 0 |
| legacy_gc | 3 | 98.59 | 0 | 0 | 0 |
| local_cleanup | 3 | 0.30 | 0 | 0 | 0 |
| manifest | 3 | 10.99 | 0 | 0 | 0 |
| row_merge_schedule | 49 | 820.82 | 245 | 0 | 0 |
| row_merge_write | 228 | 7079.40 | 0 | 922 | 0 |
| row_sst_delta | 2 | 56.37 | 0 | 8 | 0 |
| value_index_schedule | 481 | 7610.50 | 1669 | 162 | 0 |
| value_index_write | 62 | 147.56 | 0 | 28 | 0 |

Full profile window: 1246 object PUT, 699 object DELETE, 19 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 392 | 316 | 76 | — | 10.71 | 54.19 | 78.94 | 86.07 |
| commit_prune | 4 | 4 | 0 | 560.79 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| gc_delete | 4 | 4 | 0 | 7707.87 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| gc_keep | 4 | 4 | 0 | 9.53 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| gc_list | 4 | 4 | 0 | 1012.07 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| legacy_gc | 4 | 4 | 0 | 273.06 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| local_cleanup | 4 | 4 | 0 | 0.72 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| manifest | 4 | 4 | 0 | 31.32 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| row_merge_schedule | 4 | 4 | 0 | 3048.45 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| row_merge_write | 8 | 4 | 4 | 26253.66 | 80.49 | 14595.28 | 14595.28 | 14595.28 |
| row_sst_delta | 4 | 4 | 0 | 115.89 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |
| value_index_schedule | 8 | 4 | 4 | 18868.60 | 80.49 | 14595.28 | 14595.28 | 14595.28 |
| value_index_write | 4 | 4 | 0 | 189.91 | 14594.20 | 14595.28 | 14595.28 | 14595.28 |

Profiled phases intersected 58071.87 ms of 66366.46 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.284 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 0.83x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
