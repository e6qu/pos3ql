# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `403e998d80a49b906d18b3de42050c3f18cb0dfb`; binary `3db0f4c92949fc83…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

PostgreSQL baseline: version `180006`; storage: Local APFS volume /System/Volumes/Data (/tmp), isolated PostgreSQL 18 data directory; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 325.41 | 13.22 | 33.03 | 70.21 | 118.66 | 290.81 | 1600 | 0 | 0.701 | 0 |
| mixed-checkpoint-interference | 96.92 | 13.12 | 32.79 | 2412.07 | 2436.28 | 293.88 | 388 | 0 | 2.835 | 0 |
| point-concurrency-1 | 20.64 | 48.14 | 51.34 | 57.13 | 57.13 | 290.06 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 19452.95 | 0.20 | 0.34 | 0.44 | 6.32 | — | 77822 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 19196.33 | 0.20 | 0.35 | 0.45 | 7.76 | — | 76794 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 10536.58 | 0.08 | 0.11 | 0.63 | 0.63 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 3 | 1590.12 | 0 | 0 | 550 |
| gc_delete | 3 | 819.57 | 0 | 0 | 296 |
| gc_keep | 3 | 0.90 | 0 | 0 | 0 |
| gc_list | 3 | 24.92 | 0 | 0 | 0 |
| legacy_gc | 3 | 22.02 | 0 | 0 | 0 |
| manifest | 3 | 9.77 | 0 | 0 | 0 |
| row_sst | 3 | 543.34 | 19 | 117 | 0 |
| value_indexes | 3 | 1519.11 | 0 | 311 | 0 |

Full profile window: 614 object PUT, 846 object DELETE, 12 LIST.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.341 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 34.36x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
