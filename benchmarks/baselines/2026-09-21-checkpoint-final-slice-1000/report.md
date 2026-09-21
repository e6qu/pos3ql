# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `107f97f4f5e200a3ac3d5bf0ece5be91c9962a51`; binary `62de1a1b8b4b1745…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

PostgreSQL baseline: version `180006`; storage: Local APFS, isolated PostgreSQL 18 data directory; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 342.49 | 0.94 | 34.83 | 68.78 | 115.61 | 289.75 | 1721 | 0 | 0.653 | 0 |
| mixed-checkpoint-interference | 23.62 | 7.54 | 618.72 | 2387.70 | 2387.78 | 305.09 | 279 | 0 | 11.670 | 0 |
| point-concurrency-1 | 21.07 | 46.72 | 48.86 | 68.87 | 68.87 | 289.44 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 20620.02 | 0.19 | 0.30 | 0.37 | 4.81 | — | 82492 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 18561.45 | 0.19 | 0.37 | 0.76 | 28.94 | — | 74276 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 14501.69 | 0.05 | 0.09 | 0.66 | 0.66 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 9 | 1372.98 | 0 | 0 | 458 |
| gc_delete | 9 | 1748.85 | 0 | 0 | 650 |
| gc_keep | 9 | 2.75 | 0 | 0 | 0 |
| gc_list | 9 | 70.35 | 0 | 0 | 0 |
| legacy_gc | 9 | 60.97 | 0 | 0 | 0 |
| local_cleanup | 9 | 0.71 | 0 | 0 | 0 |
| manifest | 9 | 28.42 | 0 | 0 | 0 |
| row_sst | 9 | 2062.05 | 63 | 465 | 0 |
| value_indexes | 9 | 2598.21 | 5 | 505 | 0 |

Full profile window: 1047 object PUT, 1108 object DELETE, 36 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 102 | 86 | 16 | — | 3.41 | 308.18 | 354.48 | 355.00 |
| commit_prune | 32 | 24 | 8 | 5291.91 | 459.63 | 2387.72 | 2387.78 | 2387.78 |
| gc_delete | 32 | 20 | 12 | 6279.42 | 132.69 | 2387.72 | 2387.78 | 2387.78 |
| gc_keep | 46 | 33 | 13 | 9.81 | 110.98 | 2387.70 | 2387.78 | 2387.78 |
| gc_list | 36 | 24 | 12 | 251.33 | 132.65 | 2387.72 | 2387.78 | 2387.78 |
| legacy_gc | 51 | 39 | 12 | 217.32 | 434.64 | 2387.70 | 2387.78 | 2387.78 |
| local_cleanup | 32 | 24 | 8 | 2.48 | 459.63 | 2387.72 | 2387.78 | 2387.78 |
| manifest | 32 | 24 | 8 | 101.16 | 459.63 | 2387.72 | 2387.78 | 2387.78 |
| row_sst | 55 | 43 | 12 | 7199.53 | 437.20 | 2387.70 | 2387.78 | 2387.78 |
| value_indexes | 32 | 24 | 8 | 9074.76 | 459.63 | 2387.72 | 2387.78 | 2387.78 |

Profiled phases intersected 28427.74 ms of 33864.63 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.329 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 34.72x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
