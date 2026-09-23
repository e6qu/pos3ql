# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `90861f1bb8b464b8032e79fac8d6a28681aaa9ba`; binary `779e19713c2a5ea0…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, no artificial object latency.

pos3ql object store: MinIO; image `quay.io/minio/minio@sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e`; resolved `sha256:8f08aee614800…`; backing: ephemeral Docker container storage on the benchmark host; provider request metrics unavailable. This local-host timing is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL host-available baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; no container CPU or memory limit; fsync=on, full_page_writes=on, synchronous_commit=on.

PostgreSQL resource-matched baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; container limits recorded; CPU quota=12.00; memory limit=949.81 MiB; swap limit=949.81 MiB; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 215.30 | 14.67 | 50.10 | 126.19 | 479.22 | 381.41 | 2016 | 0 | — | 0 |
| mixed-checkpoint-interference | 130.43 | 11.70 | 57.84 | 557.86 | 1541.35 | 382.55 | 1236 | 0 | — | 0 |
| point-concurrency-1 | 34.12 | 21.72 | 59.16 | 110.40 | 558.01 | 378.20 | 100 | 0 | — | 0 |
| postgresql18-matched-mixed-baseline | 1939.32 | 1.07 | 5.86 | 10.72 | 522.16 | — | 16258 | 0 | — | 0 |
| postgresql18-matched-mixed-checkpoint-interference | 3152.76 | 0.53 | 4.08 | 10.05 | 248.00 | — | 25238 | 0 | — | 0 |
| postgresql18-matched-point-concurrency-1 | 2643.91 | 0.31 | 0.78 | 1.21 | 2.14 | — | 100 | 0 | — | 0 |
| postgresql18-mixed-baseline | 4102.04 | 0.41 | 3.47 | 7.71 | 43.64 | — | 32819 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 3543.78 | 0.52 | 3.87 | 7.72 | 53.11 | — | 28367 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 3680.37 | 0.22 | 0.51 | 0.79 | 1.50 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 16 | 364.47 | 0 | 0 | 198 |
| gc_delete | 8 | 163.12 | 0 | 0 | 102 |
| gc_keep | 4 | 5.54 | 0 | 0 | 0 |
| gc_list | 4 | 517.87 | 0 | 0 | 0 |
| legacy_gc | 4 | 2.56 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.46 | 0 | 0 | 0 |
| manifest | 4 | 19.77 | 0 | 0 | 0 |
| row_merge_schedule | 43 | 317.47 | 263 | 0 | 0 |
| row_merge_write | 241 | 3611.42 | 0 | 529 | 0 |
| row_sst_delta | 2 | 34.73 | 0 | 8 | 0 |
| value_index_schedule | 149 | 11.86 | 0 | 0 | 0 |
| value_index_write | 232 | 400.96 | 0 | 57 | 0 |

Provider request totals are unavailable for this profile window.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 9 | 9 | 0 | — | 6.07 | 8.97 | 8.97 | 8.97 |
| commit_prune | 12 | 12 | 0 | 34.78 | 1113.63 | 1541.35 | 1541.35 | 1541.35 |
| gc_delete | 12 | 12 | 0 | 216.70 | 1113.63 | 1541.35 | 1541.35 | 1541.35 |
| gc_keep | 12 | 12 | 0 | 13.85 | 1113.63 | 1541.35 | 1541.35 | 1541.35 |
| gc_list | 12 | 12 | 0 | 1409.67 | 1113.63 | 1541.35 | 1541.35 | 1541.35 |
| legacy_gc | 12 | 12 | 0 | 7.09 | 1113.63 | 1541.35 | 1541.35 | 1541.35 |
| local_cleanup | 12 | 12 | 0 | 1.16 | 1113.63 | 1541.35 | 1541.35 | 1541.35 |
| manifest | 12 | 12 | 0 | 64.11 | 1113.63 | 1541.35 | 1541.35 | 1541.35 |
| row_merge_schedule | 152 | 124 | 28 | 1266.76 | 13.41 | 557.78 | 1541.17 | 1541.35 |
| row_merge_write | 492 | 392 | 100 | 12813.37 | 9.59 | 65.56 | 1113.81 | 1541.35 |
| row_sst_delta | 4 | 4 | 0 | 65.81 | 1113.63 | 1113.81 | 1113.81 | 1113.81 |
| value_index_schedule | 513 | 420 | 93 | 42.40 | 10.56 | 58.26 | 72.24 | 1113.81 |
| value_index_write | 510 | 447 | 63 | 698.78 | 9.36 | 37.78 | 67.88 | 1113.81 |

Profiled phases intersected 16634.48 ms of 32006.29 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.292 | — | — | — | — |

## Derived comparisons

- checkpoint-overlap / baseline p99: 4.42x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.
- Resource-matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. The PostgreSQL container CPU and memory limits are recorded above.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
