# Object-store performance matrix

Each backend run uses the same pos3ql binary and workload shape and includes a contemporaneous vanilla PostgreSQL 18 baseline. PostgreSQL uses its recorded local durable tier; pos3ql uses the object store named below.

Run identity: `7171836a1bc60d3ef2125ccacc3fee80803d8761`; binary `779e19713c2a5ea0…`; 10000 rows, 4 clients.

## Object stores

| Backend | Implementation | Backing | Artificial latency | Request metrics |
|---|---|---|---:|---|
| fixture | tests/external/s3_test_server.py | temporary local filesystem | 2.00 ms | yes |
| minio | MinIO | ephemeral Docker container storage on the benchmark host | none | no |
| seaweedfs | SeaweedFS | ephemeral Docker container storage on the benchmark host | none | no |

## Paired results

| Backend | Engine | Scenario | completed ops | ops/s | p99 ms | max ms | object req/op | errors |
|---|---|---|---:|---:|---:|---:|---:|---:|
| fixture | pos3ql | point | 100 | 49.58 | 25.53 | 28.50 | 4.500 | 0 |
| fixture | PostgreSQL 18 | point | 100 | 3422.86 | 0.84 | 0.97 | — | 0 |
| fixture | pos3ql | mixed baseline | 1656 | 206.83 | 76.77 | 89.87 | 0.687 | 0 |
| fixture | PostgreSQL 18 | mixed baseline | 18503 | 2312.31 | 8.91 | 121.58 | — | 0 |
| fixture | pos3ql | mixed with checkpoints | 803 | 100.14 | 758.77 | 3400.93 | 0.644 | 0 |
| fixture | PostgreSQL 18 | mixed with checkpoints | 17170 | 2145.72 | 10.96 | 97.79 | — | 0 |
| minio | pos3ql | point | 100 | 24.13 | 97.79 | 118.88 | — | 0 |
| minio | PostgreSQL 18 | point | 100 | 1042.28 | 3.84 | 4.40 | — | 0 |
| minio | pos3ql | mixed baseline | 1260 | 157.15 | 103.34 | 123.33 | — | 0 |
| minio | PostgreSQL 18 | mixed baseline | 20465 | 2557.54 | 8.23 | 58.90 | — | 0 |
| minio | pos3ql | mixed with checkpoints | 602 | 75.21 | 1948.90 | 2463.25 | — | 0 |
| minio | PostgreSQL 18 | mixed with checkpoints | 23483 | 2935.24 | 6.82 | 32.73 | — | 0 |
| seaweedfs | pos3ql | point | 100 | 21.98 | 141.76 | 154.69 | — | 0 |
| seaweedfs | PostgreSQL 18 | point | 100 | 560.51 | 7.27 | 9.05 | — | 0 |
| seaweedfs | pos3ql | mixed baseline | 1241 | 154.35 | 114.11 | 149.18 | — | 0 |
| seaweedfs | PostgreSQL 18 | mixed baseline | 14301 | 1787.24 | 14.02 | 75.27 | — | 0 |
| seaweedfs | pos3ql | mixed with checkpoints | 400 | 46.94 | 2087.74 | 2862.80 | — | 0 |
| seaweedfs | PostgreSQL 18 | mixed with checkpoints | 13049 | 1629.75 | 14.94 | 152.04 | — | 0 |

## Ratios

| Backend | Scenario | pos3ql / PostgreSQL throughput | pos3ql / PostgreSQL p99 |
|---|---|---:|---:|
| fixture | point | 0.01x | 30.24x |
| fixture | mixed baseline | 0.09x | 8.62x |
| fixture | mixed with checkpoints | 0.05x | 69.24x |
| minio | point | 0.02x | 25.49x |
| minio | mixed baseline | 0.06x | 12.56x |
| minio | mixed with checkpoints | 0.03x | 285.76x |
| seaweedfs | point | 0.04x | 19.49x |
| seaweedfs | mixed baseline | 0.09x | 8.14x |
| seaweedfs | mixed with checkpoints | 0.03x | 139.76x |

Ratios compare the stated end-to-end setups. PostgreSQL has no corresponding object-request measure. Duration-bound operation counts and resulting checkpoint generation shapes can differ, and sequential local shared-host samples do not establish production ratios or rank object-store implementations.
