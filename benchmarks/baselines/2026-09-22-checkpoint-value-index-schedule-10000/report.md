# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `e3b5b80cfd4150380486803a6e92e898eb3d4300`; binary `66fdf70c9723d975…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 103.27 | 47.38 | 93.16 | 105.97 | 143.03 | 370.31 | 6394 | 0 | 2.872 | 0 |
| mixed-checkpoint-interference | 9.60 | 54.54 | 79.69 | 12362.08 | 13758.27 | 370.47 | 5780 | 0 | 29.950 | 0 |
| point-concurrency-1 | 8.24 | 73.31 | 403.85 | 406.65 | 433.83 | 369.98 | 1086 | 0 | 35.180 | 0 |
| postgresql18-mixed-baseline | 7328.60 | 0.30 | 1.53 | 3.22 | 32.52 | — | 58844 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 6403.68 | 0.33 | 1.82 | 4.00 | 107.28 | — | 51232 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 3119.70 | 0.30 | 0.42 | 0.78 | 0.91 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 8 | 521.02 | 0 | 0 | 78 |
| gc_delete | 54 | 2728.30 | 0 | 0 | 844 |
| gc_keep | 4 | 186.78 | 57 | 0 | 0 |
| gc_list | 4 | 437.34 | 0 | 0 | 0 |
| legacy_gc | 4 | 130.05 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.55 | 0 | 0 | 0 |
| manifest | 4 | 16.25 | 0 | 0 | 0 |
| row_merge_schedule | 102 | 27348.31 | 8268 | 0 | 0 |
| row_merge_write | 337 | 6489.48 | 4 | 1363 | 0 |
| row_sst_delta | 2 | 252.78 | 0 | 54 | 0 |
| value_index_schedule | 461 | 6702.17 | 1542 | 162 | 0 |
| value_index_write | 62 | 155.15 | 0 | 28 | 0 |

Full profile window: 1728 object PUT, 932 object DELETE, 25 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 384 | 308 | 76 | — | 54.45 | 77.10 | 80.06 | 90.90 |
| commit_prune | 12 | 12 | 0 | 824.78 | 12361.94 | 13758.27 | 13758.27 | 13758.27 |
| gc_delete | 12 | 12 | 0 | 9768.60 | 12361.94 | 13758.27 | 13758.27 | 13758.27 |
| gc_keep | 12 | 12 | 0 | 742.50 | 12361.94 | 13758.27 | 13758.27 | 13758.27 |
| gc_list | 12 | 12 | 0 | 1361.97 | 12361.94 | 13758.27 | 13758.27 | 13758.27 |
| legacy_gc | 12 | 12 | 0 | 374.79 | 12361.94 | 13758.27 | 13758.27 | 13758.27 |
| local_cleanup | 12 | 12 | 0 | 0.96 | 12361.94 | 13758.27 | 13758.27 | 13758.27 |
| manifest | 12 | 12 | 0 | 46.27 | 12361.94 | 13758.27 | 13758.27 | 13758.27 |
| row_merge_schedule | 16 | 12 | 4 | 104866.33 | 10505.98 | 13758.27 | 13758.27 | 13758.27 |
| row_merge_write | 12 | 12 | 0 | 25958.58 | 12361.94 | 13758.27 | 13758.27 | 13758.27 |
| row_sst_delta | 4 | 4 | 0 | 499.49 | 10505.92 | 10505.98 | 10505.98 | 10505.98 |
| value_index_schedule | 8 | 4 | 4 | 2748.14 | 370.76 | 10505.98 | 10505.98 | 10505.98 |
| value_index_write | 4 | 4 | 0 | 187.23 | 10505.92 | 10505.98 | 10505.98 | 10505.98 |

Profiled phases intersected 147379.65 ms of 166637.87 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.445 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 116.66x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
