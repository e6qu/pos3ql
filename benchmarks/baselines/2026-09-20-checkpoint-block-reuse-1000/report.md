# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `57bd13623750b4b53721589b5df09c0c81aa0c48`; binary `b46b0992055602aa…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| mixed-checkpoint-interference | 15.78 | 48.36 | 888.32 | 1929.52 | 1929.66 | 304.50 | 200 | 0 | 15.155 | 0 |
| point-concurrency-1 | 17.33 | 56.61 | 64.94 | 73.37 | 73.37 | 290.62 | 852 | 0 | 16.040 | 0 |

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| initial-start | 0.343 | 0 | 0 | 1 | 0.000 |

## Derived comparisons

- This focused run measures checkpoint overlap; compare its raw workload with a matching run.

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
