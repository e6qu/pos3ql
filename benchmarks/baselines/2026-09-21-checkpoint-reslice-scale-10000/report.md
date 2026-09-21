# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `d3f8087a0b44988ef0c88b56780040da4e817ca3`; binary `0549e211cbce8231…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 107.43 | 48.66 | 90.60 | 122.68 | 130.55 | 369.81 | 7174 | 0 | 2.919 | 0 |
| mixed-checkpoint-interference | 5.09 | 90.05 | 2879.75 | 5600.19 | 30271.24 | 371.42 | 3681 | 0 | 31.032 | 0 |
| point-concurrency-1 | 28.49 | 30.77 | 56.46 | 91.01 | 178.91 | 369.52 | 1042 | 0 | 10.170 | 0 |
| postgresql18-mixed-baseline | 6189.32 | 0.36 | 1.88 | 3.83 | 32.86 | — | 49664 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 5544.59 | 0.38 | 2.18 | 4.63 | 30.40 | — | 44359 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 3287.93 | 0.25 | 0.60 | 0.99 | 1.60 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 11 | 819.18 | 0 | 0 | 62 |
| gc_delete | 62 | 2870.14 | 0 | 0 | 907 |
| gc_keep | 10 | 184.97 | 55 | 0 | 0 |
| gc_list | 10 | 1040.84 | 0 | 0 | 0 |
| legacy_gc | 10 | 307.12 | 0 | 0 | 0 |
| local_cleanup | 10 | 1.00 | 0 | 0 | 0 |
| manifest | 10 | 36.36 | 0 | 0 | 0 |
| row_sst_delta | 13 | 918.57 | 0 | 197 | 0 |
| row_sst_full | 1 | 27500.77 | 6600 | 1337 | 0 |
| value_indexes | 14 | 41028.16 | 917 | 1017 | 0 |

Full profile window: 2815 object PUT, 981 object DELETE, 61 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 40 | 39 | 1 | — | 16.82 | 105.00 | 105.09 | 105.09 |
| commit_prune | 48 | 32 | 16 | 2435.86 | 105.57 | 3366.54 | 3366.63 | 3366.63 |
| gc_delete | 196 | 157 | 39 | 10385.09 | 101.70 | 1194.11 | 3366.57 | 3366.63 |
| gc_keep | 49 | 35 | 14 | 734.47 | 154.18 | 3366.54 | 3366.63 | 3366.63 |
| gc_list | 35 | 28 | 7 | 3396.28 | 212.31 | 3366.57 | 3366.63 | 3366.63 |
| legacy_gc | 48 | 34 | 14 | 946.23 | 83.81 | 3366.54 | 3366.63 | 3366.63 |
| local_cleanup | 28 | 20 | 8 | 2.87 | 57.43 | 3366.57 | 3366.63 | 3366.63 |
| manifest | 44 | 36 | 8 | 101.90 | 53.74 | 3366.54 | 3366.63 | 3366.63 |
| row_sst_delta | 52 | 36 | 16 | 3674.29 | 2831.99 | 5600.10 | 5600.19 | 5600.19 |
| row_sst_full | 4 | 4 | 0 | 110003.07 | 30271.13 | 30271.24 | 30271.24 | 30271.24 |
| value_indexes | 100 | 77 | 23 | 164112.17 | 2721.41 | 5600.11 | 30271.21 | 30271.24 |

Profiled phases intersected 295792.22 ms of 314503.91 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.414 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 45.65x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
