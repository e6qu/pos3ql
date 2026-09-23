# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `9ca60536fa22b565b65a255756ac75cd61bf3c90`; binary `cdb5c190daebf790…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 181.75 | 17.93 | 63.91 | 78.87 | 102.13 | 374.64 | 1760 | 0 | 0.737 | 0 |
| mixed-checkpoint-interference | 184.57 | 6.76 | 61.03 | 111.53 | 1123.99 | 84.55 | 1485 | 0 | 0.541 | 0 |
| point-concurrency-1 | 44.03 | 21.60 | 30.75 | 32.96 | 36.64 | 373.86 | 100 | 0 | 4.520 | 0 |
| postgresql18-mixed-baseline | 4223.03 | 0.49 | 3.05 | 7.28 | 44.74 | — | 33798 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 3043.16 | 0.83 | 4.06 | 7.07 | 41.20 | — | 24367 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 3035.10 | 0.26 | 0.57 | 1.38 | 2.09 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 22 | 1203.34 | 0 | 0 | 302 |
| gc_delete | 7 | 227.49 | 0 | 0 | 80 |
| gc_keep | 4 | 4.72 | 0 | 0 | 0 |
| gc_list | 4 | 398.14 | 0 | 0 | 0 |
| legacy_gc | 4 | 122.32 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.45 | 0 | 0 | 0 |
| manifest | 4 | 13.63 | 0 | 0 | 0 |
| row_merge_schedule | 40 | 121.72 | 41 | 0 | 0 |
| row_merge_write | 234 | 1009.12 | 0 | 86 | 0 |
| row_sst_delta | 2 | 751.76 | 228 | 8 | 0 |
| value_index_schedule | 12 | 2.37 | 0 | 0 | 0 |
| value_index_write | 64 | 374.41 | 0 | 66 | 0 |

Full profile window: 617 object PUT, 382 object DELETE, 24 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 1469 | 1173 | 296 | — | 6.75 | 60.11 | 79.84 | 111.63 |
| commit_prune | 12 | 12 | 0 | 730.85 | 758.51 | 1123.99 | 1123.99 | 1123.99 |
| gc_delete | 12 | 12 | 0 | 244.13 | 758.51 | 1123.99 | 1123.99 | 1123.99 |
| gc_keep | 12 | 12 | 0 | 13.76 | 758.51 | 1123.99 | 1123.99 | 1123.99 |
| gc_list | 12 | 12 | 0 | 1151.09 | 758.51 | 1123.99 | 1123.99 | 1123.99 |
| legacy_gc | 12 | 12 | 0 | 351.15 | 758.51 | 1123.99 | 1123.99 | 1123.99 |
| local_cleanup | 12 | 12 | 0 | 1.07 | 758.51 | 1123.99 | 1123.99 | 1123.99 |
| manifest | 12 | 12 | 0 | 40.41 | 758.51 | 1123.99 | 1123.99 | 1123.99 |
| row_merge_schedule | 8 | 8 | 0 | 261.29 | 327.57 | 758.57 | 758.57 | 758.57 |
| row_merge_write | 16 | 12 | 4 | 2875.26 | 327.57 | 1123.99 | 1123.99 | 1123.99 |
| row_sst_delta | 4 | 4 | 0 | 2877.60 | 1123.92 | 1123.99 | 1123.99 | 1123.99 |
| value_index_schedule | 8 | 4 | 4 | 1.27 | 37.47 | 1123.99 | 1123.99 | 1123.99 |
| value_index_write | 4 | 4 | 0 | 205.36 | 1123.92 | 1123.99 | 1123.99 | 1123.99 |

Profiled phases intersected 8753.25 ms of 32163.15 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.343 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 1.41x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
