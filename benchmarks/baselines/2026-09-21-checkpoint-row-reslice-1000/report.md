# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `6591700f931989fa2a8e0be84d05782097b4b159`; binary `2327dfcff37ad875…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume on host APFS; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 299.22 | 13.68 | 54.76 | 78.35 | 127.21 | 291.02 | 1570 | 0 | 0.698 | 0 |
| mixed-checkpoint-interference | 42.89 | 41.87 | 412.37 | 717.67 | 717.90 | 306.17 | 242 | 0 | 5.450 | 0 |
| point-concurrency-1 | 18.47 | 53.54 | 56.77 | 96.59 | 96.59 | 290.70 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 2390.42 | 0.93 | 5.46 | 9.92 | 49.49 | — | 9542 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 1685.97 | 0.69 | 10.15 | 21.83 | 61.74 | — | 6745 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 604.46 | 1.12 | 4.22 | 5.17 | 5.17 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 7 | 219.86 | 0 | 0 | 38 |
| gc_delete | 18 | 758.56 | 0 | 0 | 239 |
| gc_keep | 7 | 2.11 | 0 | 0 | 0 |
| gc_list | 7 | 55.82 | 0 | 0 | 0 |
| legacy_gc | 7 | 48.26 | 0 | 0 | 0 |
| local_cleanup | 7 | 0.55 | 0 | 0 | 0 |
| manifest | 7 | 26.22 | 0 | 0 | 0 |
| row_sst | 11 | 1891.33 | 0 | 427 | 0 |
| value_indexes | 11 | 828.40 | 76 | 70 | 0 |

Full profile window: 590 object PUT, 282 object DELETE, 28 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 32 | 28 | 4 | — | 7.14 | 342.17 | 342.24 | 342.24 |
| commit_prune | 36 | 24 | 12 | 694.42 | 65.66 | 652.21 | 652.22 | 652.22 |
| gc_delete | 63 | 52 | 11 | 2316.99 | 56.18 | 651.98 | 652.22 | 652.22 |
| gc_keep | 31 | 25 | 6 | 7.30 | 45.23 | 652.21 | 652.22 | 652.22 |
| gc_list | 28 | 25 | 3 | 193.26 | 59.38 | 652.21 | 652.22 | 652.22 |
| legacy_gc | 36 | 23 | 13 | 165.39 | 65.66 | 652.21 | 652.22 | 652.22 |
| local_cleanup | 24 | 19 | 5 | 1.80 | 27.13 | 652.21 | 652.22 | 652.22 |
| manifest | 36 | 31 | 5 | 89.87 | 27.05 | 652.21 | 652.22 | 652.22 |
| row_sst | 40 | 28 | 12 | 6944.69 | 218.11 | 717.67 | 717.90 | 717.90 |
| value_indexes | 69 | 53 | 16 | 2473.91 | 62.10 | 717.66 | 717.90 | 717.90 |

Profiled phases intersected 12887.62 ms of 18651.11 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.286 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 9.16x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
