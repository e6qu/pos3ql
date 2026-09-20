# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `9282b6b50a108dfb910717a9a371027a2ea09e14`; binary `236271daf1dcebfc…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-checkpoint-interference | 10.31 | 63.24 | 1661.30 | 2217.77 | 2285.02 | 303.52 | 200 | 0 | 19.865 | 0 |
| point-concurrency-1 | 18.34 | 54.26 | 60.63 | 63.99 | 63.99 | 289.94 | 852 | 0 | 16.040 | 0 |

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.308 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- This focused run measures checkpoint overlap; compare its raw workload with a matching run.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
