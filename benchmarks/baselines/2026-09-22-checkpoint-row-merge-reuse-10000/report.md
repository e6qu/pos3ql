# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `6caf6eff37b898e2af782f08c6e260574ae6a40e`; binary `f5bd624b880c98fb…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 188.77 | 17.55 | 63.08 | 75.20 | 96.75 | 374.33 | 1817 | 0 | 0.778 | 0 |
| mixed-checkpoint-interference | 102.80 | 6.34 | 40.50 | 62.48 | 4132.58 | 374.73 | 825 | 0 | 1.836 | 0 |
| point-concurrency-1 | 41.04 | 20.64 | 40.39 | 59.22 | 222.84 | 373.95 | 100 | 0 | 4.570 | 0 |
| postgresql18-mixed-baseline | 2142.43 | 1.12 | 6.03 | 10.75 | 92.99 | — | 17147 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 1774.10 | 0.89 | 6.60 | 16.29 | 546.28 | — | 14194 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 2683.02 | 0.27 | 0.99 | 1.57 | 3.23 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 16 | 958.87 | 0 | 0 | 224 |
| gc_delete | 14 | 696.00 | 0 | 0 | 194 |
| gc_keep | 3 | 3.48 | 0 | 0 | 0 |
| gc_list | 3 | 318.81 | 0 | 0 | 0 |
| legacy_gc | 3 | 93.56 | 0 | 0 | 0 |
| local_cleanup | 3 | 0.35 | 0 | 0 | 0 |
| manifest | 3 | 10.64 | 0 | 0 | 0 |
| row_merge_schedule | 40 | 145.05 | 43 | 0 | 0 |
| row_merge_write | 234 | 1065.04 | 0 | 90 | 0 |
| row_sst_delta | 2 | 56.52 | 0 | 8 | 0 |
| value_index_schedule | 398 | 3869.66 | 910 | 162 | 0 |
| value_index_write | 62 | 180.46 | 0 | 42 | 0 |

Full profile window: 641 object PUT, 423 object DELETE, 18 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 813 | 649 | 164 | — | 6.33 | 39.07 | 53.03 | 62.48 |
| commit_prune | 8 | 8 | 0 | 543.25 | 849.30 | 4132.58 | 4132.58 | 4132.58 |
| gc_delete | 8 | 8 | 0 | 1054.22 | 849.30 | 4132.58 | 4132.58 | 4132.58 |
| gc_keep | 8 | 8 | 0 | 9.16 | 849.30 | 4132.58 | 4132.58 | 4132.58 |
| gc_list | 8 | 8 | 0 | 862.36 | 849.30 | 4132.58 | 4132.58 | 4132.58 |
| legacy_gc | 8 | 8 | 0 | 249.25 | 849.30 | 4132.58 | 4132.58 | 4132.58 |
| local_cleanup | 8 | 8 | 0 | 0.81 | 849.30 | 4132.58 | 4132.58 | 4132.58 |
| manifest | 8 | 8 | 0 | 29.19 | 849.30 | 4132.58 | 4132.58 | 4132.58 |
| row_merge_schedule | 8 | 8 | 0 | 308.34 | 849.30 | 4132.58 | 4132.58 | 4132.58 |
| row_merge_write | 12 | 8 | 4 | 2997.87 | 848.57 | 4132.58 | 4132.58 | 4132.58 |
| row_sst_delta | 4 | 4 | 0 | 117.98 | 4132.57 | 4132.58 | 4132.58 | 4132.58 |
| value_index_schedule | 8 | 4 | 4 | 13421.02 | 37.33 | 4132.58 | 4132.58 | 4132.58 |
| value_index_write | 4 | 4 | 0 | 168.50 | 4132.57 | 4132.58 | 4132.58 | 4132.58 |

Profiled phases intersected 19761.94 ms of 32068.71 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.361 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 0.83x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
