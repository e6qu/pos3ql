# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `8fe85e06cad590b9a3cac2392801837270f538fa`; binary `16e785f9b1e91efd…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; storage: Local APFS, isolated PostgreSQL 18 data directory; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 321.80 | 12.84 | 43.16 | 75.12 | 107.03 | 290.86 | 1623 | 0 | 0.691 | 0 |
| mixed-checkpoint-interference | 180.00 | 13.32 | 32.40 | 624.60 | 920.19 | 303.59 | 725 | 0 | 1.368 | 0 |
| point-concurrency-1 | 21.90 | 45.66 | 47.39 | 52.71 | 52.71 | 290.55 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 16744.82 | 0.21 | 0.43 | 0.61 | 14.73 | — | 66987 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 18381.11 | 0.20 | 0.39 | 0.59 | 6.54 | — | 73534 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 8765.39 | 0.08 | 0.23 | 0.64 | 0.64 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 4 | 727.36 | 0 | 0 | 254 |
| gc_delete | 4 | 563.91 | 0 | 0 | 218 |
| gc_keep | 4 | 1.19 | 0 | 0 | 0 |
| gc_list | 4 | 30.26 | 0 | 0 | 0 |
| legacy_gc | 4 | 25.71 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.28 | 0 | 0 | 0 |
| manifest | 4 | 12.49 | 0 | 0 | 0 |
| row_sst | 3 | 506.47 | 21 | 117 | 0 |
| value_indexes | 3 | 1034.94 | 0 | 224 | 0 |

Full profile window: 765 object PUT, 475 object DELETE, 16 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 713 | 570 | 143 | — | 13.28 | 31.34 | 36.96 | 71.70 |
| commit_prune | 12 | 8 | 4 | 250.95 | 624.73 | 920.19 | 920.19 | 920.19 |
| gc_delete | 12 | 8 | 4 | 894.33 | 624.73 | 920.19 | 920.19 | 920.19 |
| gc_keep | 12 | 8 | 4 | 3.55 | 624.73 | 920.19 | 920.19 | 920.19 |
| gc_list | 12 | 8 | 4 | 85.27 | 624.73 | 920.19 | 920.19 | 920.19 |
| legacy_gc | 12 | 8 | 4 | 72.89 | 624.73 | 920.19 | 920.19 | 920.19 |
| local_cleanup | 12 | 8 | 4 | 0.78 | 624.73 | 920.19 | 920.19 | 920.19 |
| manifest | 12 | 8 | 4 | 38.10 | 624.73 | 920.19 | 920.19 | 920.19 |
| row_sst | 8 | 4 | 4 | 1385.74 | 533.67 | 644.98 | 644.98 | 644.98 |
| value_indexes | 8 | 4 | 4 | 1906.74 | 533.67 | 644.98 | 644.98 | 644.98 |

Profiled phases intersected 4638.35 ms of 16074.19 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.329 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 8.32x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
