# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `b55e5b148ca9fb67afce01c1694a0b3f49d6a8bd`; binary `779e19713c2a5ea0…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 225.20 | 14.90 | 54.92 | 64.23 | 73.52 | 381.55 | 2277 | 0 | 0.667 | 0 |
| mixed-checkpoint-interference | 208.87 | 8.99 | 38.18 | 69.34 | 869.44 | 381.84 | 2320 | 0 | 0.548 | 0 |
| point-concurrency-1 | 51.52 | 19.36 | 23.14 | 23.46 | 25.20 | 378.56 | 100 | 0 | 4.480 | 0 |
| postgresql18-mixed-baseline | 5942.27 | 0.39 | 1.92 | 4.00 | 35.61 | — | 47543 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 5037.17 | 0.45 | 2.50 | 4.59 | 27.87 | — | 39218 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 4099.24 | 0.22 | 0.35 | 0.45 | 0.70 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 24 | 1156.93 | 0 | 0 | 334 |
| gc_delete | 8 | 293.88 | 0 | 0 | 109 |
| gc_keep | 4 | 4.71 | 0 | 0 | 0 |
| gc_list | 4 | 384.93 | 0 | 0 | 0 |
| legacy_gc | 4 | 116.20 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.46 | 0 | 0 | 0 |
| manifest | 4 | 13.06 | 0 | 0 | 0 |
| row_merge_schedule | 62 | 617.39 | 216 | 0 | 0 |
| row_merge_write | 233 | 1284.92 | 0 | 74 | 0 |
| row_sst_delta | 2 | 33.34 | 0 | 8 | 0 |
| value_index_schedule | 12 | 2.38 | 0 | 0 | 0 |
| value_index_write | 64 | 285.53 | 0 | 72 | 0 |

Full profile window: 659 object PUT, 453 object DELETE, 24 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 1656 | 1324 | 332 | — | 8.98 | 37.97 | 49.63 | 69.98 |
| commit_prune | 12 | 12 | 0 | 725.59 | 704.46 | 869.44 | 869.44 | 869.44 |
| gc_delete | 12 | 12 | 0 | 483.89 | 704.46 | 869.44 | 869.44 | 869.44 |
| gc_keep | 12 | 12 | 0 | 13.89 | 704.46 | 869.44 | 869.44 | 869.44 |
| gc_list | 12 | 12 | 0 | 1147.50 | 704.46 | 869.44 | 869.44 | 869.44 |
| legacy_gc | 12 | 12 | 0 | 345.15 | 704.46 | 869.44 | 869.44 | 869.44 |
| local_cleanup | 12 | 12 | 0 | 1.16 | 704.46 | 869.44 | 869.44 | 869.44 |
| manifest | 12 | 12 | 0 | 39.41 | 704.46 | 869.44 | 869.44 | 869.44 |
| row_merge_schedule | 16 | 12 | 4 | 726.76 | 671.02 | 869.44 | 869.44 | 869.44 |
| row_merge_write | 12 | 12 | 0 | 5140.10 | 704.46 | 869.44 | 869.44 | 869.44 |
| row_sst_delta | 4 | 4 | 0 | 54.00 | 704.46 | 704.52 | 704.52 | 704.52 |
| value_index_schedule | 8 | 4 | 4 | 1.31 | 51.31 | 704.52 | 704.52 | 704.52 |
| value_index_write | 4 | 4 | 0 | 211.37 | 704.46 | 704.52 | 704.52 | 704.52 |

Profiled phases intersected 8890.12 ms of 32001.75 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.346 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 1.08x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
