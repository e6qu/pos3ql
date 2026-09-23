# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `90861f1bb8b464b8032e79fac8d6a28681aaa9ba`; binary `779e19713c2a5ea0…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, no artificial object latency.

pos3ql object store: SeaweedFS; image `chrislusf/seaweedfs@sha256:08d516132314207d10c8e37cbffc1f32b147d870169688734cc61c6231625b62`; resolved `sha256:b51a342057651…`; backing: ephemeral Docker container storage on the benchmark host; provider request metrics unavailable. This local-host timing is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL host-available baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; no container CPU or memory limit; fsync=on, full_page_writes=on, synchronous_commit=on.

PostgreSQL resource-matched baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; container limits recorded; CPU quota=12.00; memory limit=949.81 MiB; swap limit=949.81 MiB; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 276.55 | 11.53 | 39.59 | 60.25 | 90.20 | 380.27 | 2519 | 0 | — | 0 |
| mixed-checkpoint-interference | 130.27 | 16.13 | 49.25 | 441.96 | 1325.12 | 381.91 | 1235 | 0 | — | 0 |
| point-concurrency-1 | 43.06 | 21.97 | 39.66 | 42.14 | 48.37 | 377.38 | 100 | 0 | — | 0 |
| postgresql18-matched-mixed-baseline | 3660.41 | 0.51 | 3.81 | 7.49 | 55.49 | — | 29294 | 0 | — | 0 |
| postgresql18-matched-mixed-checkpoint-interference | 3488.84 | 0.55 | 3.77 | 7.33 | 86.34 | — | 27916 | 0 | — | 0 |
| postgresql18-matched-point-concurrency-1 | 3536.59 | 0.22 | 0.50 | 0.90 | 3.10 | — | 100 | 0 | — | 0 |
| postgresql18-mixed-baseline | 3765.74 | 0.48 | 3.78 | 7.21 | 42.78 | — | 30137 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 3288.44 | 0.53 | 4.26 | 8.86 | 95.17 | — | 26320 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 1846.63 | 0.38 | 1.49 | 2.29 | 2.83 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 15 | 172.36 | 0 | 0 | 186 |
| gc_delete | 13 | 184.29 | 0 | 0 | 175 |
| gc_keep | 4 | 4.54 | 0 | 0 | 0 |
| gc_list | 4 | 132.92 | 0 | 0 | 0 |
| legacy_gc | 4 | 3.18 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.52 | 0 | 0 | 0 |
| manifest | 4 | 16.94 | 0 | 0 | 0 |
| row_merge_schedule | 46 | 502.52 | 275 | 0 | 0 |
| row_merge_write | 239 | 3760.18 | 0 | 595 | 0 |
| row_sst_delta | 2 | 39.54 | 0 | 8 | 0 |
| value_index_schedule | 150 | 12.91 | 0 | 0 | 0 |
| value_index_write | 256 | 408.39 | 0 | 64 | 0 |

Provider request totals are unavailable for this profile window.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 12 | 11 | 1 | — | 3.26 | 21.55 | 21.55 | 21.55 |
| commit_prune | 12 | 12 | 0 | 106.56 | 974.51 | 1325.12 | 1325.12 | 1325.12 |
| gc_delete | 12 | 12 | 0 | 81.48 | 974.51 | 1325.12 | 1325.12 | 1325.12 |
| gc_keep | 12 | 12 | 0 | 13.62 | 974.51 | 1325.12 | 1325.12 | 1325.12 |
| gc_list | 12 | 12 | 0 | 364.00 | 974.51 | 1325.12 | 1325.12 | 1325.12 |
| legacy_gc | 12 | 12 | 0 | 10.19 | 974.51 | 1325.12 | 1325.12 | 1325.12 |
| local_cleanup | 12 | 12 | 0 | 1.27 | 974.51 | 1325.12 | 1325.12 | 1325.12 |
| manifest | 12 | 12 | 0 | 61.39 | 974.51 | 1325.12 | 1325.12 | 1325.12 |
| row_merge_schedule | 156 | 124 | 32 | 2005.91 | 18.99 | 441.91 | 1325.08 | 1325.12 |
| row_merge_write | 466 | 370 | 96 | 13660.22 | 20.55 | 52.51 | 974.58 | 1325.12 |
| row_sst_delta | 4 | 4 | 0 | 103.58 | 974.51 | 974.58 | 974.58 | 974.58 |
| value_index_schedule | 507 | 415 | 92 | 44.90 | 16.93 | 42.49 | 74.64 | 974.58 |
| value_index_write | 515 | 450 | 65 | 928.32 | 11.34 | 35.49 | 74.64 | 974.58 |

Profiled phases intersected 17381.46 ms of 32087.79 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.476 | — | — | — | — |

## Derived comparisons

- checkpoint-overlap / baseline p99: 7.34x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.
- Resource-matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. The PostgreSQL container CPU and memory limits are recorded above.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
