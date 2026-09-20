# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `d1aba82ea63a83ff9eed8febd9ca247691ca666f`; binary `90b21cb47c575984…`; arm64, 12 logical CPUs; 1000 rows, 4 clients, 1024 MiB fixed disk cache, 120.0 s query timeout, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

PostgreSQL baseline: version `180006`; storage: Local APFS volume /System/Volumes/Data (/tmp), PostgreSQL data directory on /dev/disk3s5; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| analytical-scan | 56.30 | 5.59 | 42.17 | 42.17 | 42.17 | 292.59 | 0 | 10 | 1.333 | 0 |
| cold-object-brin-inclusion | 1162.89 | 0.62 | 1.43 | 8.70 | 8.70 | 276.98 | 50 | 0 | 0.040 | 0 |
| cold-object-brin-point | 1804.81 | 0.34 | 1.05 | 8.12 | 8.12 | 276.80 | 50 | 0 | 0.040 | 0 |
| cold-object-gin-array-overlap | 2729.09 | 0.32 | 0.82 | 0.90 | 0.90 | 277.73 | 50 | 0 | 0.000 | 0 |
| cold-object-gin-array | 1130.19 | 0.33 | 4.01 | 7.57 | 7.57 | 277.61 | 50 | 0 | 0.140 | 0 |
| cold-object-gin-jsonb-path | 1287.15 | 0.32 | 3.04 | 6.29 | 6.29 | 278.14 | 50 | 0 | 0.140 | 0 |
| cold-object-gin-jsonb | 1257.76 | 0.29 | 3.34 | 6.27 | 6.27 | 278.06 | 50 | 0 | 0.160 | 0 |
| cold-object-gin-tsvector | 1115.65 | 0.37 | 4.22 | 7.99 | 7.99 | 277.89 | 50 | 0 | 0.140 | 0 |
| cold-object-gist-inclusion | 1138.38 | 0.40 | 4.12 | 7.94 | 7.94 | 277.17 | 50 | 0 | 0.120 | 0 |
| cold-object-gist-knn | 2411.23 | 0.38 | 0.50 | 1.06 | 1.06 | 277.56 | 50 | 0 | 0.000 | 0 |
| cold-object-gist-multirange | 944.87 | 0.43 | 4.24 | 8.15 | 8.15 | 277.27 | 50 | 0 | 0.160 | 0 |
| cold-object-gist-network | 1274.83 | 0.32 | 3.96 | 7.62 | 7.62 | 277.33 | 50 | 0 | 0.120 | 0 |
| cold-object-gist-spatial | 1025.97 | 0.56 | 4.25 | 8.25 | 8.25 | 277.55 | 50 | 0 | 0.100 | 0 |
| cold-object-gist-tsvector | 285.72 | 3.68 | 7.49 | 12.16 | 12.16 | 278.02 | 50 | 0 | 0.980 | 0 |
| cold-object-join-probe | 137.63 | 7.04 | 8.43 | 10.07 | 10.07 | 264.38 | 3200 | 0 | 0.000 | 0 |
| cold-object-ordered-limit | 1066.92 | 0.89 | 1.21 | 1.58 | 1.58 | 276.52 | 50 | 0 | 0.000 | 0 |
| cold-object-point | 4811.33 | 0.75 | 1.30 | 1.87 | 1.99 | 276.70 | 200 | 0 | 0.000 | 0 |
| cold-object-spgist-knn | 2682.39 | 0.33 | 0.56 | 1.24 | 1.24 | 278.58 | 50 | 0 | 0.000 | 0 |
| cold-object-spgist-network | 1402.40 | 0.26 | 4.25 | 5.83 | 5.83 | 278.48 | 50 | 0 | 0.120 | 0 |
| cold-object-spgist-prefix | 1232.56 | 0.36 | 1.18 | 17.09 | 17.09 | 278.33 | 50 | 0 | 0.040 | 0 |
| cold-object-spgist-range | 1256.61 | 0.36 | 3.90 | 6.86 | 6.86 | 278.45 | 50 | 0 | 0.120 | 0 |
| cold-object-spgist-spatial | 1095.28 | 0.53 | 3.99 | 7.26 | 7.26 | 278.52 | 50 | 0 | 0.100 | 0 |
| cold-object-tail-range | 45.99 | 21.28 | 22.01 | 39.38 | 39.38 | 276.23 | 100 | 0 | 0.040 | 0 |
| concurrent-insert | 104.70 | 38.04 | 41.33 | 49.39 | 49.54 | 292.58 | 0 | 0 | 1.500 | 0 |
| concurrent-update | 102.80 | 37.43 | 40.56 | 67.95 | 67.96 | 292.31 | 200 | 0 | 1.500 | 0 |
| mixed-baseline | 399.56 | 0.54 | 35.30 | 39.50 | 43.09 | 292.59 | 200 | 0 | 0.420 | 0 |
| mixed-checkpoint-interference | 8.45 | 54.09 | 1427.38 | 4355.29 | 4393.88 | 306.11 | 321 | 0 | 29.510 | 0 |
| point-concurrency-1 | 20.98 | 47.39 | 48.78 | 56.81 | 56.81 | 290.05 | 852 | 0 | 16.040 | 0 |
| postgresql18-insert | 9855.94 | 0.28 | 1.13 | 1.93 | 2.30 | — | 0 | 0 | — | 0 |
| postgresql18-join-probe | 18503.46 | 0.18 | 0.27 | 1.41 | 1.47 | — | 0 | 200 | — | 0 |
| postgresql18-mixed | 11642.60 | 0.17 | 1.73 | 2.24 | 2.31 | — | 200 | 0 | — | 0 |
| postgresql18-ordered-limit | 2289.71 | 1.72 | 2.39 | 2.68 | 2.82 | — | 200 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 7541.57 | 0.12 | 0.15 | 0.80 | 0.80 | — | 50 | 0 | — | 0 |
| postgresql18-point | 16185.10 | 0.21 | 0.37 | 1.13 | 1.43 | — | 200 | 0 | — | 0 |
| postgresql18-scan | 2395.69 | 0.20 | 0.82 | 0.82 | 0.82 | — | 0 | 3 | — | 0 |
| postgresql18-tail-range | 13577.39 | 0.19 | 0.33 | 4.53 | 4.58 | — | 200 | 0 | — | 0 |
| warm-disk-point | 5055.18 | 0.72 | 1.45 | 1.57 | 1.73 | 282.72 | 200 | 0 | 0.000 | 0 |
| warm-memory-brin-inclusion | 2826.55 | 1.35 | 1.98 | 2.44 | 2.61 | 290.36 | 200 | 0 | 0.000 | 0 |
| warm-memory-brin-point | 7450.49 | 0.51 | 0.66 | 1.21 | 1.52 | 290.14 | 200 | 0 | 0.000 | 0 |
| warm-memory-gin-array-overlap | 6411.12 | 0.56 | 1.14 | 1.74 | 1.75 | 290.81 | 200 | 0 | 0.000 | 0 |
| warm-memory-gin-array | 7845.83 | 0.48 | 0.63 | 1.28 | 1.54 | 290.77 | 200 | 0 | 0.000 | 0 |
| warm-memory-gin-jsonb-path | 6233.86 | 0.60 | 1.00 | 1.62 | 1.66 | 290.88 | 200 | 0 | 0.000 | 0 |
| warm-memory-gin-jsonb | 7902.90 | 0.47 | 0.68 | 1.33 | 1.64 | 290.86 | 200 | 0 | 0.000 | 0 |
| warm-memory-gin-tsvector | 5591.56 | 0.67 | 1.11 | 1.39 | 1.62 | 290.83 | 200 | 0 | 0.000 | 0 |
| warm-memory-gist-inclusion | 5026.58 | 0.77 | 1.11 | 1.49 | 1.69 | 290.36 | 200 | 0 | 0.000 | 0 |
| warm-memory-gist-knn | 3582.10 | 1.06 | 1.62 | 2.33 | 2.40 | 290.73 | 200 | 0 | 0.000 | 0 |
| warm-memory-gist-multirange | 4195.57 | 0.89 | 1.58 | 1.91 | 1.98 | 290.48 | 200 | 0 | 0.000 | 0 |
| warm-memory-gist-network | 8053.92 | 0.46 | 0.83 | 1.13 | 1.38 | 290.48 | 200 | 0 | 0.000 | 0 |
| warm-memory-gist-spatial | 2515.36 | 1.55 | 2.20 | 2.43 | 2.80 | 290.59 | 200 | 0 | 0.000 | 0 |
| warm-memory-gist-tsvector | 3657.75 | 1.05 | 1.37 | 1.83 | 2.10 | 290.83 | 200 | 0 | 0.000 | 0 |
| warm-memory-join-probe | 178.31 | 17.38 | 17.99 | 266.54 | 279.95 | 292.06 | 14304 | 0 | 0.080 | 0 |
| warm-memory-ordered-limit | 2903.49 | 1.14 | 2.81 | 3.06 | 3.15 | 292.03 | 200 | 0 | 0.000 | 0 |
| warm-memory-point | 934.69 | 0.43 | 46.88 | 48.54 | 48.91 | 290.09 | 460 | 0 | 0.480 | 0 |
| warm-memory-spgist-knn | 3283.21 | 1.20 | 1.96 | 2.36 | 2.52 | 291.72 | 200 | 0 | 0.000 | 0 |
| warm-memory-spgist-network | 7722.67 | 0.47 | 0.77 | 1.11 | 1.45 | 291.72 | 200 | 0 | 0.000 | 0 |
| warm-memory-spgist-prefix | 1164.11 | 0.67 | 8.81 | 78.17 | 138.04 | 291.72 | 390 | 0 | 0.330 | 0 |
| warm-memory-spgist-range | 5144.39 | 0.73 | 1.08 | 1.53 | 1.85 | 291.72 | 200 | 0 | 0.000 | 0 |
| warm-memory-spgist-spatial | 2609.39 | 1.49 | 1.82 | 2.44 | 2.64 | 291.72 | 200 | 0 | 0.000 | 0 |
| warm-memory-tail-range | 146.95 | 17.20 | 18.01 | 514.37 | 514.48 | 291.97 | 526 | 0 | 0.160 | 0 |

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| cold-object-join-recovery | 0.029 | 6 | 24 | 0 | 0.158 |
| cold-object-recovery | 0.030 | 6 | 31 | 0 | 0.160 |
| initial-start | 0.340 | 0 | 0 | 1 | 0.000 |
| warm-disk-recovery | 0.028 | 6 | 18 | 0 | 0.039 |

## Derived comparisons

- warm-disk / warm-memory p99: 0.03x
- cold-object / warm-memory p99: 0.04x
- checkpoint-overlap / baseline p99: 110.26x
- pos3ql / PostgreSQL 18 warm point throughput: 0.06x
- pos3ql concurrent / single-client point throughput: 44.56x
- PostgreSQL 18 concurrent / single-client point throughput: 2.15x
- pos3ql / PostgreSQL 18 insert throughput: 0.01x
- pos3ql / PostgreSQL 18 scan throughput: 0.02x

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
