# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `0821645dcc49e6110a2ef60f6a2ccc309e1ca7e6`; binary `fc58aaaa68703dae…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

PostgreSQL baseline: version `180006`; storage: Local APFS volume /System/Volumes/Data (/tmp), isolated PostgreSQL 18 data directory; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-baseline | 259.26 | 0.81 | 64.88 | 72.39 | 80.58 | 291.16 | 258 | 0 | 0.900 | 0 |
| mixed-checkpoint-interference | 83.04 | 13.05 | 44.24 | 1447.94 | 1463.36 | 293.17 | 200 | 0 | 3.310 | 0 |
| point-concurrency-1 | 20.63 | 47.83 | 53.53 | 57.91 | 57.91 | 290.86 | 852 | 0 | 16.040 | 0 |
| postgresql18-mixed-baseline | 17943.39 | 0.15 | 0.35 | 2.76 | 2.80 | — | 200 | 0 | — | 0 |
| postgresql18-mixed-checkpoint-interference | 15099.71 | 0.20 | 0.55 | 1.51 | 2.11 | — | 200 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 10260.88 | 0.08 | 0.11 | 0.61 | 0.61 | — | 50 | 0 | — | 0 |

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.301 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- checkpoint-overlap / baseline p99: 20.00x
- Matched checkpoint commands completed: pos3ql 3, PostgreSQL 18 3. This short shared-host run compares SQL workloads on distinct persistence tiers; the p99 samples above do not establish production ratios.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
