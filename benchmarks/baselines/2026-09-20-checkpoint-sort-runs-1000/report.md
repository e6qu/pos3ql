# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `7bb1dcaf12f981afdedc9a803063e61ecc0d6d08`; binary `fc58aaaa68703dae…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-checkpoint-interference | 28.74 | 38.49 | 530.52 | 1259.12 | 1259.18 | 302.92 | 203 | 0 | 8.830 | 0 |
| point-concurrency-1 | 20.87 | 47.58 | 50.18 | 54.30 | 54.30 | 288.89 | 852 | 0 | 16.040 | 0 |

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.346 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- This focused run measures checkpoint overlap; compare its raw workload with a matching run.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
