# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `7171836a1bc60d3ef2125ccacc3fee80803d8761`; binary `779e19713c2a5ea0…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, no artificial object latency.

pos3ql object store: MinIO; image `quay.io/minio/minio@sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e`; resolved `sha256:8f08aee614800…`; backing: ephemeral Docker container storage on the benchmark host; provider request metrics unavailable. This local-host timing is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 157.15 | 19.83 | 71.36 | 103.34 | 123.33 | 380.89 | 1570 | 0 | — | 0 |
| mixed-checkpoint-interference | 75.21 | 9.83 | 63.39 | 1948.90 | 2463.25 | 381.30 | 602 | 0 | — | 0 |
| point-concurrency-1 | 24.13 | 36.59 | 74.05 | 97.79 | 118.88 | 378.30 | 100 | 0 | — | 0 |
| postgresql18-mixed-baseline | 2557.54 | 0.99 | 4.83 | 8.23 | 58.90 | — | 20465 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 2935.24 | 0.91 | 4.22 | 6.82 | 32.73 | — | 23483 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 1042.28 | 0.78 | 2.51 | 3.84 | 4.40 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 8 | 149.19 | 0 | 0 | 68 |
| gc_delete | 6 | 195.78 | 0 | 0 | 59 |
| gc_keep | 4 | 4.37 | 0 | 0 | 0 |
| gc_list | 4 | 447.24 | 0 | 0 | 0 |
| legacy_gc | 4 | 4.15 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.40 | 0 | 0 | 0 |
| manifest | 4 | 55.83 | 0 | 0 | 0 |
| row_merge_schedule | 43 | 470.27 | 249 | 0 | 0 |
| row_merge_write | 237 | 5333.09 | 0 | 497 | 0 |
| row_sst_delta | 2 | 24.96 | 0 | 8 | 0 |
| value_index_schedule | 70 | 3.98 | 0 | 0 | 0 |
| value_index_write | 121 | 302.89 | 0 | 36 | 0 |

Provider request totals are unavailable for this profile window.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 0 | 0 | 0 | — | — | — | — | — |
| commit_prune | 12 | 12 | 0 | 96.48 | 1948.90 | 2463.25 | 2463.25 | 2463.25 |
| gc_delete | 12 | 12 | 0 | 442.68 | 1948.90 | 2463.25 | 2463.25 | 2463.25 |
| gc_keep | 12 | 12 | 0 | 13.53 | 1948.90 | 2463.25 | 2463.25 | 2463.25 |
| gc_list | 12 | 12 | 0 | 1204.36 | 1948.90 | 2463.25 | 2463.25 | 2463.25 |
| legacy_gc | 12 | 12 | 0 | 14.87 | 1948.90 | 2463.25 | 2463.25 | 2463.25 |
| local_cleanup | 12 | 12 | 0 | 1.09 | 1948.90 | 2463.25 | 2463.25 | 2463.25 |
| manifest | 12 | 12 | 0 | 182.29 | 1948.90 | 2463.25 | 2463.25 | 2463.25 |
| row_merge_schedule | 148 | 120 | 28 | 1875.83 | 21.42 | 629.41 | 2463.25 | 2463.25 |
| row_merge_write | 466 | 372 | 94 | 18950.87 | 9.23 | 70.46 | 1948.93 | 2463.25 |
| row_sst_delta | 4 | 4 | 0 | 44.65 | 1948.90 | 1948.93 | 1948.93 | 1948.93 |
| value_index_schedule | 241 | 229 | 12 | 13.07 | 9.43 | 42.57 | 1948.90 | 1948.93 |
| value_index_write | 237 | 236 | 1 | 471.27 | 9.18 | 24.88 | 1948.90 | 1948.93 |

Profiled phases intersected 23310.98 ms of 32013.16 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.369 | — | — | — | — |

## Derived comparisons

- checkpoint-overlap / baseline p99: 18.86x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
