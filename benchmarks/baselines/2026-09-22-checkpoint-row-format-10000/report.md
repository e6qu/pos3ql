# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `931d83916651d874133e96d0f39a873fc7d1d33a`; binary `0da26a7fb6dfeb0a…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 104.10 | 37.46 | 83.09 | 113.41 | 115.47 | 371.03 | 6374 | 0 | 2.978 | 0 |
| mixed-checkpoint-interference | 13.01 | 39.08 | 62.94 | 89.55 | 27018.53 | 371.33 | 4855 | 0 | 24.820 | 0 |
| point-concurrency-1 | 56.84 | 17.34 | 21.13 | 23.72 | 26.07 | 370.67 | 100 | 0 | 4.480 | 0 |
| postgresql18-mixed-baseline | 6520.77 | 0.33 | 1.79 | 3.78 | 49.20 | — | 52169 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 5164.86 | 0.34 | 2.77 | 5.48 | 33.19 | — | 41325 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 2915.47 | 0.31 | 0.45 | 0.84 | 1.51 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 7 | 403.87 | 0 | 0 | 78 |
| gc_delete | 39 | 1639.90 | 0 | 0 | 608 |
| gc_keep | 3 | 6.77 | 1 | 0 | 0 |
| gc_list | 3 | 326.07 | 0 | 0 | 0 |
| legacy_gc | 3 | 95.70 | 0 | 0 | 0 |
| local_cleanup | 3 | 0.30 | 0 | 0 | 0 |
| manifest | 3 | 10.12 | 0 | 0 | 0 |
| row_merge_schedule | 37 | 642.90 | 238 | 0 | 0 |
| row_merge_write | 440 | 21306.20 | 5782 | 906 | 0 |
| row_sst_delta | 2 | 49.91 | 0 | 8 | 0 |
| value_index_schedule | 522 | 6976.92 | 2074 | 162 | 0 |
| value_index_write | 62 | 129.83 | 0 | 28 | 0 |

Full profile window: 1224 object PUT, 686 object DELETE, 19 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 392 | 316 | 76 | — | 38.91 | 60.19 | 82.34 | 89.55 |
| commit_prune | 4 | 4 | 0 | 529.60 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| gc_delete | 4 | 4 | 0 | 5577.41 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| gc_keep | 4 | 4 | 0 | 21.19 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| gc_list | 4 | 4 | 0 | 916.38 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| legacy_gc | 4 | 4 | 0 | 239.66 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| local_cleanup | 4 | 4 | 0 | 0.75 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| manifest | 4 | 4 | 0 | 27.50 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| row_merge_schedule | 4 | 4 | 0 | 2428.31 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| row_merge_write | 8 | 4 | 4 | 82430.75 | 81.48 | 27018.53 | 27018.53 | 27018.53 |
| row_sst_delta | 4 | 4 | 0 | 97.16 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |
| value_index_schedule | 8 | 4 | 4 | 15535.00 | 81.48 | 27018.53 | 27018.53 | 27018.53 |
| value_index_write | 4 | 4 | 0 | 176.38 | 27018.49 | 27018.53 | 27018.53 | 27018.53 |

Profiled phases intersected 107980.10 ms of 123013.61 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.341 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 0.79x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
