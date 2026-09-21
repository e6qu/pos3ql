# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `11186fc03efd2147c671674e711281d536c97872`; binary `076b76bcdb5fb2b5…`; arm64, 12 logical CPUs; 10000 rows, 4 clients, 2048 MiB fixed disk cache, 300.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

Mixed workloads run for at least 8.0 seconds and 100 operations per client; operation counts may differ across engines.

Each engine completes an unmeasured settling checkpoint before the checkpoint-interference window.

PostgreSQL baseline: version `180006`; image `sha256:d8a40176c29aa…`; storage: Docker-managed local volume; host backing unspecified; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 22.26 | 36.30 | 217.33 | 2779.41 | 5423.36 | 369.84 | 523 | 0 | 4.410 | 0 |
| mixed-checkpoint-interference | 11.25 | 73.28 | 2802.80 | 4030.29 | 6242.31 | 94.30 | 4621 | 0 | 17.305 | 0 |
| point-concurrency-1 | 6.98 | 72.37 | 355.82 | 396.33 | 472.18 | 369.06 | 1626 | 0 | 40.710 | 0 |
| postgresql18-mixed-baseline | 5645.20 | 0.32 | 1.46 | 3.61 | 295.35 | — | 45164 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 6322.07 | 0.34 | 1.74 | 3.90 | 79.31 | — | 50583 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 3854.93 | 0.25 | 0.36 | 0.45 | 0.80 | — | 100 | 0 | — | 0 |

## Checkpoint phases

Profiled pos3ql build; totals cover explicit and automatic checkpoint work from the workload start through server stop.
Times sum phase spans and are not query latency or a cross-system metric. The profile request window includes cleanup after the timed workload ends.

| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |
|---|---:|---:|---:|---:|---:|
| commit_prune | 10 | 655.90 | 0 | 0 | 52 |
| gc_delete | 66 | 3397.34 | 0 | 0 | 962 |
| gc_keep | 8 | 9.92 | 0 | 0 | 0 |
| gc_list | 8 | 830.48 | 0 | 0 | 0 |
| legacy_gc | 8 | 247.50 | 0 | 0 | 0 |
| local_cleanup | 8 | 0.57 | 0 | 0 | 0 |
| manifest | 8 | 30.29 | 0 | 0 | 0 |
| row_merge_schedule | 64 | 16101.27 | 4712 | 0 | 0 |
| row_merge_write | 207 | 4236.11 | 0 | 844 | 0 |
| row_sst_delta | 5 | 358.84 | 0 | 75 | 0 |
| value_indexes | 5 | 15147.69 | 408 | 363 | 0 |

Full profile window: 1368 object PUT, 1019 object DELETE, 50 LIST.

### Foreground overlap

Operations are grouped when their client-observed interval intersects a phase on the common realtime axis. One operation can intersect multiple phases.

| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| no phase overlap | 12 | 12 | 0 | — | 0.54 | 2.29 | 2.29 | 2.29 |
| commit_prune | 36 | 28 | 8 | 1658.05 | 129.79 | 6241.94 | 6242.31 | 6242.31 |
| gc_delete | 74 | 62 | 12 | 6427.38 | 103.80 | 6241.81 | 6242.31 | 6242.31 |
| gc_keep | 36 | 28 | 8 | 29.93 | 200.64 | 6241.94 | 6242.31 | 6242.31 |
| gc_list | 24 | 20 | 4 | 2362.50 | 207.00 | 6241.94 | 6242.31 | 6242.31 |
| legacy_gc | 36 | 28 | 8 | 718.05 | 116.41 | 6241.94 | 6242.31 | 6242.31 |
| local_cleanup | 24 | 20 | 4 | 1.69 | 64.20 | 6241.94 | 6242.31 | 6242.31 |
| manifest | 36 | 32 | 4 | 89.86 | 404.47 | 6241.94 | 6242.31 | 6242.31 |
| row_merge_schedule | 180 | 142 | 38 | 51712.52 | 291.55 | 3635.12 | 6241.94 | 6242.31 |
| row_merge_write | 119 | 99 | 20 | 10181.67 | 69.76 | 3635.12 | 6241.94 | 6242.31 |
| row_sst_delta | 16 | 16 | 0 | 1140.00 | 2820.50 | 4030.29 | 4030.29 | 4030.29 |
| value_indexes | 32 | 22 | 10 | 49744.59 | 456.67 | 4030.17 | 4030.29 | 4030.29 |

Profiled phases intersected 124066.25 ms of 142228.12 ms summed foreground latency.

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.300 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 1.45x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu, compiled by gcc (Debian 14.2.0-19) 14.2.0, 64-bit`
