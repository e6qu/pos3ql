# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `90861f1bb8b464b8032e79fac8d6a28681aaa9ba`; binary `779e19713c2a5ea0…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql object store: tests/external/s3_test_server.py; backing: temporary local filesystem; exact request metrics available. This local-host timing is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL host-available baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; no container CPU or memory limit; fsync=on, full_page_writes=on, synchronous_commit=on.

PostgreSQL resource-matched baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; container limits recorded; CPU quota=12.00; memory limit=949.81 MiB; swap limit=949.81 MiB; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 202.10 | 17.50 | 60.31 | 83.16 | 119.96 | 84.38 | 1938 | 0 | 0.684 | 0 |
| mixed-checkpoint-interference | 215.23 | 9.29 | 39.17 | 56.13 | 834.37 | 85.31 | 2370 | 0 | 0.548 | 0 |
| point-concurrency-1 | 40.65 | 21.49 | 36.90 | 54.03 | 66.67 | 70.55 | 100 | 0 | 4.440 | 0 |
| postgresql18-matched-mixed-baseline | 6442.94 | 0.33 | 1.86 | 4.11 | 17.46 | — | 51546 | 0 | — | 0 |
| postgresql18-matched-mixed-checkpoint-interference | 6394.09 | 0.33 | 1.82 | 3.98 | 20.28 | — | 49575 | 0 | — | 0 |
| postgresql18-matched-point-concurrency-1 | 3223.34 | 0.27 | 0.45 | 0.83 | 1.53 | — | 100 | 0 | — | 0 |
| postgresql18-mixed-baseline | 6977.88 | 0.31 | 1.59 | 3.54 | 56.19 | — | 55859 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 6241.95 | 0.33 | 1.93 | 4.23 | 24.12 | — | 49951 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 3329.09 | 0.25 | 0.52 | 1.02 | 1.95 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 25 | 1447.07 | 0 | 0 | 346 |
| gc_delete | 8 | 288.99 | 0 | 0 | 93 |
| gc_keep | 4 | 4.81 | 0 | 0 | 0 |
| gc_list | 4 | 384.58 | 0 | 0 | 0 |
| legacy_gc | 4 | 119.77 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.46 | 0 | 0 | 0 |
| manifest | 4 | 14.65 | 0 | 0 | 0 |
| row_merge_schedule | 69 | 874.90 | 253 | 0 | 0 |
| row_merge_write | 235 | 892.40 | 0 | 56 | 0 |
| row_sst_delta | 2 | 37.67 | 0 | 8 | 0 |
| value_index_schedule | 12 | 2.75 | 0 | 0 | 0 |
| value_index_write | 64 | 327.25 | 0 | 74 | 0 |

Full profile window: 661 object PUT, 448 object DELETE, 24 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 1709 | 1365 | 344 | — | 9.26 | 38.94 | 49.03 | 56.18 |
| commit_prune | 12 | 12 | 0 | 726.21 | 657.58 | 834.37 | 834.37 | 834.37 |
| gc_delete | 12 | 12 | 0 | 338.20 | 657.58 | 834.37 | 834.37 | 834.37 |
| gc_keep | 12 | 12 | 0 | 14.08 | 657.58 | 834.37 | 834.37 | 834.37 |
| gc_list | 12 | 12 | 0 | 1143.68 | 657.58 | 834.37 | 834.37 | 834.37 |
| legacy_gc | 12 | 12 | 0 | 348.25 | 657.58 | 834.37 | 834.37 | 834.37 |
| local_cleanup | 12 | 12 | 0 | 1.16 | 657.58 | 834.37 | 834.37 | 834.37 |
| manifest | 12 | 12 | 0 | 43.46 | 657.58 | 834.37 | 834.37 | 834.37 |
| row_merge_schedule | 16 | 12 | 4 | 1506.15 | 498.07 | 834.37 | 834.37 | 834.37 |
| row_merge_write | 12 | 12 | 0 | 3570.09 | 657.58 | 834.37 | 834.37 | 834.37 |
| row_sst_delta | 4 | 4 | 0 | 58.79 | 834.27 | 834.37 | 834.37 | 834.37 |
| value_index_schedule | 8 | 4 | 4 | 1.26 | 69.23 | 834.37 | 834.37 | 834.37 |
| value_index_write | 4 | 4 | 0 | 207.98 | 834.27 | 834.37 | 834.37 | 834.37 |

Profiled phases intersected 7959.30 ms of 32040.80 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.345 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 0.67x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.
- Resource-matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. The PostgreSQL container CPU and memory limits are recorded above.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
