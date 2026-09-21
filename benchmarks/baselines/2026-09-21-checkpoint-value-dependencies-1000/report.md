# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `9207132c471729b77243f579bad867179494afd0`; binary `61af48726e49fb7a…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 4.0 seconds and 50 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume on host APFS; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 298.72 | 13.28 | 38.87 | 80.10 | 136.63 | 286.02 | 1557 | 0 | 0.741 | 0 |
| mixed-checkpoint-interference | 215.37 | 11.24 | 30.63 | 328.69 | 1191.76 | 298.38 | 866 | 0 | 0.950 | 0 |
| point-concurrency-1 | 20.46 | 47.50 | 56.47 | 58.07 | 58.07 | 285.72 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 2473.34 | 0.94 | 5.48 | 11.06 | 35.15 | — | 9898 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 2615.29 | 0.93 | 4.90 | 9.81 | 32.41 | — | 10485 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 2814.17 | 0.30 | 0.72 | 1.20 | 1.20 | — | 50 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 20 | 985.67 | 0 | 0 | 272 |
| gc_delete | 4 | 94.51 | 0 | 0 | 28 |
| gc_keep | 4 | 1.61 | 0 | 0 | 0 |
| gc_list | 4 | 31.23 | 0 | 0 | 0 |
| legacy_gc | 4 | 30.86 | 0 | 0 | 0 |
| local_cleanup | 4 | 0.58 | 0 | 0 | 0 |
| manifest | 4 | 29.54 | 0 | 0 | 0 |
| row_sst | 3 | 534.38 | 18 | 105 | 0 |
| value_indexes | 3 | 192.69 | 0 | 24 | 0 |

Full profile window: 576 object PUT, 307 object DELETE, 16 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 854 | 681 | 173 | — | 4.60 | 29.38 | 44.24 | 55.50 |
| commit_prune | 12 | 10 | 2 | 378.60 | 328.82 | 1191.76 | 1191.76 | 1191.76 |
| gc_delete | 12 | 10 | 2 | 239.68 | 328.82 | 1191.76 | 1191.76 | 1191.76 |
| gc_keep | 12 | 10 | 2 | 5.29 | 328.82 | 1191.76 | 1191.76 | 1191.76 |
| gc_list | 12 | 10 | 2 | 93.13 | 328.82 | 1191.76 | 1191.76 | 1191.76 |
| legacy_gc | 12 | 10 | 2 | 95.93 | 328.82 | 1191.76 | 1191.76 | 1191.76 |
| local_cleanup | 12 | 10 | 2 | 1.39 | 328.82 | 1191.76 | 1191.76 | 1191.76 |
| manifest | 12 | 10 | 2 | 70.95 | 328.82 | 1191.76 | 1191.76 | 1191.76 |
| row_sst | 8 | 6 | 2 | 1428.23 | 328.69 | 341.44 | 341.44 | 341.44 |
| value_indexes | 8 | 6 | 2 | 435.43 | 328.69 | 341.44 | 341.44 | 341.44 |

Profiled phases intersected 2748.63 ms of 16031.52 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.341 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 4.10x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
