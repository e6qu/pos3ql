# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `7171836a1bc60d3ef2125ccacc3fee80803d8761`; binary `779e19713c2a5ea0…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, no artificial object latency.

pos3ql object store: SeaweedFS; image `chrislusf/seaweedfs@sha256:08d516132314207d10c8e37cbffc1f32b147d870169688734cc61c6231625b62`; resolved `sha256:b51a342057651…`; backing: ephemeral Docker container storage on the benchmark host; provider request metrics unavailable. This local-host timing is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 154.35 | 18.05 | 72.56 | 114.11 | 149.18 | 379.17 | 1563 | 0 | — | 0 |
| mixed-checkpoint-interference | 46.94 | 22.08 | 98.69 | 2087.74 | 2862.80 | 379.53 | 400 | 0 | — | 0 |
| point-concurrency-1 | 21.98 | 37.43 | 116.40 | 141.76 | 154.69 | 376.66 | 100 | 0 | — | 0 |
| postgresql18-mixed-baseline | 1787.24 | 1.27 | 7.56 | 14.02 | 75.27 | — | 14301 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 1629.75 | 1.40 | 7.76 | 14.94 | 152.04 | — | 13049 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 560.51 | 1.38 | 4.11 | 7.27 | 9.05 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 7 | 158.46 | 0 | 0 | 62 |
| gc_delete | 5 | 92.51 | 0 | 0 | 45 |
| gc_keep | 4 | 4.65 | 0 | 0 | 0 |
| gc_list | 4 | 381.75 | 0 | 0 | 0 |
| legacy_gc | 4 | 9.93 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.38 | 0 | 0 | 0 |
| manifest | 4 | 23.22 | 0 | 0 | 0 |
| row_merge_schedule | 49 | 976.89 | 249 | 0 | 0 |
| row_merge_write | 218 | 6254.10 | 0 | 494 | 0 |
| row_sst_delta | 2 | 35.20 | 0 | 8 | 0 |
| value_index_schedule | 48 | 2.37 | 0 | 0 | 0 |
| value_index_write | 99 | 229.19 | 0 | 28 | 0 |

Provider request totals are unavailable for this profile window.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 0 | 0 | 0 | — | — | — | — | — |
| commit_prune | 12 | 12 | 0 | 55.49 | 2087.62 | 2862.80 | 2862.80 | 2862.80 |
| gc_delete | 12 | 12 | 0 | 315.43 | 2087.62 | 2862.80 | 2862.80 | 2862.80 |
| gc_keep | 12 | 12 | 0 | 13.90 | 2087.62 | 2862.80 | 2862.80 | 2862.80 |
| gc_list | 12 | 12 | 0 | 1238.00 | 2087.62 | 2862.80 | 2862.80 | 2862.80 |
| legacy_gc | 12 | 12 | 0 | 23.17 | 2087.62 | 2862.80 | 2862.80 | 2862.80 |
| local_cleanup | 12 | 12 | 0 | 1.01 | 2087.62 | 2862.80 | 2862.80 | 2862.80 |
| manifest | 12 | 12 | 0 | 43.31 | 2087.62 | 2862.80 | 2862.80 | 2862.80 |
| row_merge_schedule | 148 | 120 | 28 | 3894.22 | 37.87 | 577.24 | 2862.77 | 2862.80 |
| row_merge_write | 264 | 212 | 52 | 21217.99 | 9.67 | 282.54 | 2862.72 | 2862.80 |
| row_sst_delta | 4 | 4 | 0 | 32.43 | 2087.62 | 2087.74 | 2087.74 | 2087.74 |
| value_index_schedule | 160 | 122 | 38 | 7.83 | 32.43 | 98.69 | 2087.66 | 2087.74 |
| value_index_write | 160 | 159 | 1 | 283.53 | 9.54 | 51.01 | 2087.66 | 2087.74 |

Profiled phases intersected 27126.31 ms of 34078.21 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.342 | — | — | — | — |

## Derived comparisons

- checkpoint-overlap / baseline p99: 18.30x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
