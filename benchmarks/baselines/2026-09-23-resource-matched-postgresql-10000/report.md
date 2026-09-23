# Object-store performance matrix

Each backend run uses the same pos3ql binary and workload shape and includes host-available and resource-matched vanilla PostgreSQL 18 baselines. PostgreSQL uses its recorded local durable tier; pos3ql uses the object store named below.

Run identity: `90861f1bb8b464b8032e79fac8d6a28681aaa9ba`; binary `779e19713c2a5ea0…`; 10000 rows, 4 clients.

## Object stores

| Backend | Implementation | Backing | Artificial latency | Request metrics |
|---|---|---|---:|---|
| fixture | tests/external/s3_test_server.py | temporary local filesystem | 2.00 ms | yes |
| minio | MinIO | ephemeral Docker container storage on the benchmark host | none | no |
| seaweedfs | SeaweedFS | ephemeral Docker container storage on the benchmark host | none | no |

## PostgreSQL resource parity

The matched container receives the same CPU count available to pos3ql and an exact cgroup memory limit equal to pos3ql's fixed startup plan. Matching CPU availability does not imply equal CPU consumption.

| Backend run | pos3ql CPUs | pos3ql fixed MiB | Matched PG CPU quota | Matched PG MiB | Matched PG swap MiB |
|---|---:|---:|---:|---:|---:|
| fixture | 12 | 949.81 | 12.00 | 949.81 | 949.81 |
| minio | 12 | 949.81 | 12.00 | 949.81 | 949.81 |
| seaweedfs | 12 | 949.81 | 12.00 | 949.81 | 949.81 |

## Paired results

| Backend | Engine | Scenario | completed ops | ops/s | p99 ms | max ms | object req/op | errors |
|---|---|---|---:|---:|---:|---:|---:|---:|
| fixture | pos3ql | point | 100 | 40.65 | 54.03 | 66.67 | 4.440 | 0 |
| fixture | PostgreSQL 18 host available | point | 100 | 3329.09 | 1.02 | 1.95 | — | 0 |
| fixture | PostgreSQL 18 resource matched | point | 100 | 3223.34 | 0.83 | 1.53 | — | 0 |
| fixture | pos3ql | mixed baseline | 1621 | 202.10 | 83.16 | 119.96 | 0.684 | 0 |
| fixture | PostgreSQL 18 host available | mixed baseline | 55859 | 6977.88 | 3.54 | 56.19 | — | 0 |
| fixture | PostgreSQL 18 resource matched | mixed baseline | 51546 | 6442.94 | 4.11 | 17.46 | — | 0 |
| fixture | pos3ql | mixed with checkpoints | 1725 | 215.23 | 56.13 | 834.37 | 0.548 | 0 |
| fixture | PostgreSQL 18 host available | mixed with checkpoints | 49951 | 6241.95 | 4.23 | 24.12 | — | 0 |
| fixture | PostgreSQL 18 resource matched | mixed with checkpoints | 51168 | 6394.09 | 3.98 | 20.28 | — | 0 |
| minio | pos3ql | point | 100 | 34.12 | 110.40 | 558.01 | — | 0 |
| minio | PostgreSQL 18 host available | point | 100 | 3680.37 | 0.79 | 1.50 | — | 0 |
| minio | PostgreSQL 18 resource matched | point | 100 | 2643.91 | 1.21 | 2.14 | — | 0 |
| minio | pos3ql | mixed baseline | 1727 | 215.30 | 126.19 | 479.22 | — | 0 |
| minio | PostgreSQL 18 host available | mixed baseline | 32819 | 4102.04 | 7.71 | 43.64 | — | 0 |
| minio | PostgreSQL 18 resource matched | mixed baseline | 16258 | 1939.32 | 10.72 | 522.16 | — | 0 |
| minio | pos3ql | mixed with checkpoints | 1044 | 130.43 | 557.86 | 1541.35 | — | 0 |
| minio | PostgreSQL 18 host available | mixed with checkpoints | 28367 | 3543.78 | 7.72 | 53.11 | — | 0 |
| minio | PostgreSQL 18 resource matched | mixed with checkpoints | 25238 | 3152.76 | 10.05 | 248.00 | — | 0 |
| seaweedfs | pos3ql | point | 100 | 43.06 | 42.14 | 48.37 | — | 0 |
| seaweedfs | PostgreSQL 18 host available | point | 100 | 1846.63 | 2.29 | 2.83 | — | 0 |
| seaweedfs | PostgreSQL 18 resource matched | point | 100 | 3536.59 | 0.90 | 3.10 | — | 0 |
| seaweedfs | pos3ql | mixed baseline | 2217 | 276.55 | 60.25 | 90.20 | — | 0 |
| seaweedfs | PostgreSQL 18 host available | mixed baseline | 30137 | 3765.74 | 7.21 | 42.78 | — | 0 |
| seaweedfs | PostgreSQL 18 resource matched | mixed baseline | 29294 | 3660.41 | 7.49 | 55.49 | — | 0 |
| seaweedfs | pos3ql | mixed with checkpoints | 1046 | 130.27 | 441.96 | 1325.12 | — | 0 |
| seaweedfs | PostgreSQL 18 host available | mixed with checkpoints | 26320 | 3288.44 | 8.86 | 95.17 | — | 0 |
| seaweedfs | PostgreSQL 18 resource matched | mixed with checkpoints | 27916 | 3488.84 | 7.33 | 86.34 | — | 0 |

## Ratios

| Backend | Scenario | throughput / host available | p99 / host available | throughput / resource matched | p99 / resource matched |
|---|---|---:|---:|---:|---:|
| fixture | point | 0.01x | 52.74x | 0.01x | 65.44x |
| fixture | mixed baseline | 0.03x | 23.50x | 0.03x | 20.22x |
| fixture | mixed with checkpoints | 0.03x | 13.26x | 0.03x | 14.09x |
| minio | point | 0.01x | 140.22x | 0.01x | 91.29x |
| minio | mixed baseline | 0.05x | 16.37x | 0.11x | 11.77x |
| minio | mixed with checkpoints | 0.04x | 72.28x | 0.04x | 55.50x |
| seaweedfs | point | 0.02x | 18.44x | 0.01x | 46.78x |
| seaweedfs | mixed baseline | 0.07x | 8.35x | 0.08x | 8.05x |
| seaweedfs | mixed with checkpoints | 0.04x | 49.90x | 0.04x | 60.31x |

Ratios compare the stated end-to-end setups. PostgreSQL has no corresponding object-request measure. Duration-bound operation counts and resulting checkpoint generation shapes can differ, and sequential local shared-host samples do not establish production ratios or rank object-store implementations.
