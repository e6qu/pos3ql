# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `7171836a1bc60d3ef2125ccacc3fee80803d8761`; binary `779e19713c2a5ea0…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql object store: tests/external/s3_test_server.py; backing: temporary local filesystem; exact request metrics available. This local-host timing is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 206.83 | 16.43 | 61.09 | 76.77 | 89.87 | 95.73 | 2006 | 0 | 0.687 | 0 |
| mixed-checkpoint-interference | 100.14 | 7.00 | 44.32 | 758.77 | 3400.93 | 96.28 | 804 | 0 | 0.644 | 0 |
| point-concurrency-1 | 49.58 | 19.63 | 23.96 | 25.53 | 28.50 | 245.00 | 100 | 0 | 4.500 | 0 |
| postgresql18-mixed-baseline | 2312.31 | 1.10 | 5.35 | 8.91 | 121.58 | — | 18503 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 2145.72 | 1.09 | 5.94 | 10.96 | 97.79 | — | 17170 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 3422.86 | 0.25 | 0.68 | 0.84 | 0.97 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 15 | 744.44 | 0 | 0 | 186 |
| gc_delete | 6 | 178.19 | 0 | 0 | 63 |
| gc_keep | 4 | 57.88 | 0 | 0 | 0 |
| gc_list | 4 | 427.98 | 0 | 0 | 0 |
| legacy_gc | 4 | 120.35 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.45 | 0 | 0 | 0 |
| manifest | 4 | 13.58 | 0 | 0 | 0 |
| row_merge_schedule | 68 | 741.91 | 248 | 0 | 0 |
| row_merge_write | 235 | 3699.26 | 0 | 58 | 0 |
| row_sst_delta | 2 | 28.79 | 0 | 8 | 0 |
| value_index_schedule | 12 | 1.39 | 0 | 0 | 0 |
| value_index_write | 64 | 180.92 | 0 | 42 | 0 |

Full profile window: 391 object PUT, 251 object DELETE, 24 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 787 | 629 | 158 | — | 6.85 | 38.25 | 62.33 | 81.72 |
| commit_prune | 12 | 12 | 0 | 732.42 | 880.11 | 3400.93 | 3400.93 | 3400.93 |
| gc_delete | 12 | 12 | 0 | 341.42 | 880.11 | 3400.93 | 3400.93 | 3400.93 |
| gc_keep | 12 | 12 | 0 | 227.26 | 880.11 | 3400.93 | 3400.93 | 3400.93 |
| gc_list | 12 | 12 | 0 | 1334.03 | 880.11 | 3400.93 | 3400.93 | 3400.93 |
| legacy_gc | 12 | 12 | 0 | 361.49 | 880.11 | 3400.93 | 3400.93 | 3400.93 |
| local_cleanup | 12 | 12 | 0 | 1.12 | 880.11 | 3400.93 | 3400.93 | 3400.93 |
| manifest | 12 | 12 | 0 | 41.05 | 880.11 | 3400.93 | 3400.93 | 3400.93 |
| row_merge_schedule | 16 | 12 | 4 | 1170.76 | 758.77 | 3400.93 | 3400.93 | 3400.93 |
| row_merge_write | 12 | 12 | 0 | 14797.45 | 880.11 | 3400.93 | 3400.93 | 3400.93 |
| row_sst_delta | 4 | 4 | 0 | 50.46 | 880.11 | 880.12 | 880.12 | 880.12 |
| value_index_schedule | 8 | 4 | 4 | 1.31 | 85.79 | 880.12 | 880.12 | 880.12 |
| value_index_write | 4 | 4 | 0 | 190.99 | 880.11 | 880.12 | 880.12 | 880.12 |

Profiled phases intersected 19249.77 ms of 32034.98 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.349 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 9.88x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
