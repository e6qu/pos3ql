# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `c0c46276a7b376af00b015e81e7252cb4239dc76`; binary `4717b6b17d8e3ba7…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume on host APFS; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 324.47 | 13.01 | 45.09 | 69.56 | 113.48 | 290.09 | 1703 | 0 | 0.699 | 0 |
| mixed-checkpoint-interference | 47.15 | 34.21 | 423.72 | 983.23 | 1000.96 | 305.41 | 251 | 0 | 5.345 | 0 |
| point-concurrency-1 | 21.11 | 47.03 | 53.33 | 54.45 | 54.45 | 289.80 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 2468.01 | 1.02 | 4.84 | 9.55 | 84.39 | — | 9876 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 2007.87 | 1.20 | 6.19 | 11.82 | 47.97 | — | 8038 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 1229.03 | 0.72 | 1.55 | 2.19 | 2.19 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 8 | 293.89 | 0 | 0 | 52 |
| gc_delete | 19 | 695.13 | 0 | 0 | 253 |
| gc_keep | 7 | 3.37 | 0 | 0 | 0 |
| gc_list | 7 | 54.47 | 0 | 0 | 0 |
| legacy_gc | 7 | 45.70 | 0 | 0 | 0 |
| local_cleanup | 7 | 0.51 | 0 | 0 | 0 |
| manifest | 7 | 23.61 | 0 | 0 | 0 |
| row_sst | 12 | 1596.56 | 0 | 362 | 0 |
| value_indexes | 12 | 753.72 | 71 | 82 | 0 |

Full profile window: 563 object PUT, 306 object DELETE, 28 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 41 | 36 | 5 | — | 1.32 | 128.01 | 128.17 | 128.17 |
| commit_prune | 31 | 22 | 9 | 886.83 | 115.05 | 983.28 | 1000.96 | 1000.96 |
| gc_delete | 61 | 47 | 14 | 1796.51 | 46.39 | 983.10 | 1000.96 | 1000.96 |
| gc_keep | 26 | 21 | 5 | 12.15 | 83.73 | 983.28 | 1000.96 | 1000.96 |
| gc_list | 20 | 14 | 6 | 159.72 | 275.47 | 983.28 | 1000.96 | 1000.96 |
| legacy_gc | 28 | 23 | 5 | 134.47 | 45.14 | 983.28 | 1000.96 | 1000.96 |
| local_cleanup | 20 | 14 | 6 | 1.57 | 275.47 | 983.28 | 1000.96 | 1000.96 |
| manifest | 28 | 22 | 6 | 66.57 | 28.62 | 983.28 | 1000.96 | 1000.96 |
| row_sst | 40 | 33 | 7 | 5711.74 | 94.91 | 983.23 | 1000.96 | 1000.96 |
| value_indexes | 71 | 56 | 15 | 2262.81 | 64.33 | 983.10 | 1000.96 | 1000.96 |

Profiled phases intersected 11032.36 ms of 16966.12 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.412 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 14.13x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
