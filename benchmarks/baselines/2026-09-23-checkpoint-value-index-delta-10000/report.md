# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `fe63361245a922d81c2f7c2104a7e486d6c32dfb`; binary `b0c01b8eda124c3f…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 201.65 | 16.53 | 63.21 | 80.81 | 120.94 | 376.61 | 1946 | 0 | 0.693 | 0 |
| mixed-checkpoint-interference | 171.41 | 6.26 | 49.57 | 108.72 | 1081.98 | 376.69 | 1372 | 0 | 0.708 | 0 |
| point-concurrency-1 | 49.78 | 19.90 | 24.46 | 25.93 | 26.51 | 374.39 | 100 | 0 | 4.520 | 0 |
| postgresql18-mixed-baseline | 2152.75 | 0.73 | 7.15 | 14.38 | 49.22 | — | 17246 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 2786.20 | 0.62 | 5.07 | 9.86 | 55.77 | — | 22292 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 706.64 | 0.37 | 6.13 | 21.60 | 23.06 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 29 | 1416.53 | 0 | 0 | 404 |
| gc_delete | 7 | 239.71 | 0 | 0 | 80 |
| gc_keep | 4 | 4.62 | 0 | 0 | 0 |
| gc_list | 4 | 382.96 | 0 | 0 | 0 |
| legacy_gc | 4 | 120.79 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.44 | 0 | 0 | 0 |
| manifest | 4 | 12.48 | 0 | 0 | 0 |
| row_merge_schedule | 42 | 144.83 | 47 | 0 | 0 |
| row_merge_write | 235 | 1018.91 | 0 | 100 | 0 |
| row_sst_delta | 2 | 734.49 | 228 | 8 | 0 |
| value_index_schedule | 12 | 2.06 | 0 | 0 | 0 |
| value_index_write | 64 | 260.51 | 0 | 62 | 0 |

Full profile window: 780 object PUT, 486 object DELETE, 24 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 1356 | 1084 | 272 | — | 6.24 | 46.86 | 65.42 | 108.79 |
| commit_prune | 12 | 12 | 0 | 718.74 | 788.55 | 1081.98 | 1081.98 | 1081.98 |
| gc_delete | 12 | 12 | 0 | 322.91 | 788.55 | 1081.98 | 1081.98 | 1081.98 |
| gc_keep | 12 | 12 | 0 | 13.41 | 788.55 | 1081.98 | 1081.98 | 1081.98 |
| gc_list | 12 | 12 | 0 | 1144.04 | 788.55 | 1081.98 | 1081.98 | 1081.98 |
| legacy_gc | 12 | 12 | 0 | 346.05 | 788.55 | 1081.98 | 1081.98 | 1081.98 |
| local_cleanup | 12 | 12 | 0 | 1.12 | 788.55 | 1081.98 | 1081.98 | 1081.98 |
| manifest | 12 | 12 | 0 | 38.62 | 788.55 | 1081.98 | 1081.98 | 1081.98 |
| row_merge_schedule | 8 | 8 | 0 | 305.93 | 313.24 | 788.61 | 788.61 | 788.61 |
| row_merge_write | 16 | 12 | 4 | 2760.15 | 313.24 | 1081.98 | 1081.98 | 1081.98 |
| row_sst_delta | 4 | 4 | 0 | 2811.83 | 1081.89 | 1081.98 | 1081.98 | 1081.98 |
| value_index_schedule | 8 | 4 | 4 | 1.21 | 46.04 | 1081.98 | 1081.98 | 1081.98 |
| value_index_write | 4 | 4 | 0 | 187.87 | 1081.89 | 1081.98 | 1081.98 | 1081.98 |

Profiled phases intersected 8651.87 ms of 32001.05 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.343 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 1.35x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
