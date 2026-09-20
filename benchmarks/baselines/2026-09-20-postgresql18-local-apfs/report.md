# pos3ql performance report

Raw JSON files are the evidence; this report derives comparisons without hiding errors.

Run identity: `1ef10592879fd403b8c036a1fcb5c43afb82ea7f`; binary `0bbce28e6878b6ea…`; arm64, 12 logical CPUs; 256 rows, 4 clients, 2.0 ms injected object latency.

pos3ql uses an instrumented object-store fixture backed by local temporary storage. Timing from this run is exploratory.

PostgreSQL baseline: version `180006`; storage: Local APFS volume /System/Volumes/Data (/tmp), PostgreSQL data directory on /dev/disk3s5; fsync enabled; fsync=on, full_page_writes=on, synchronous_commit=on.

| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| analytical-scan | 116.66 | 8.25 | 8.81 | 8.81 | 8.81 | 274.00 | 0 | 4 | 0.000 | 0 |
| cold-object-brin-inclusion | 1206.72 | 0.39 | 1.06 | 8.26 | 8.26 | 29.91 | 20 | 0 | 0.100 | 0 |
| cold-object-brin-point | 1468.51 | 0.27 | 0.44 | 7.96 | 7.96 | 27.27 | 20 | 0 | 0.100 | 0 |
| cold-object-gin-array-overlap | 3425.24 | 0.26 | 0.38 | 0.58 | 0.58 | 30.20 | 20 | 0 | 0.000 | 0 |
| cold-object-gin-array | 253.58 | 0.30 | 4.95 | 67.57 | 67.57 | 30.19 | 20 | 0 | 0.150 | 0 |
| cold-object-gin-jsonb-path | 895.39 | 0.31 | 4.22 | 12.46 | 12.46 | 30.39 | 20 | 0 | 0.150 | 0 |
| cold-object-gin-jsonb | 767.00 | 0.24 | 3.66 | 17.87 | 17.87 | 30.33 | 20 | 0 | 0.150 | 0 |
| cold-object-gin-tsvector | 989.06 | 0.34 | 4.06 | 9.46 | 9.46 | 30.33 | 20 | 0 | 0.150 | 0 |
| cold-object-gist-inclusion | 1047.92 | 0.36 | 4.12 | 7.87 | 7.87 | 29.95 | 20 | 0 | 0.150 | 0 |
| cold-object-gist-knn | 2610.54 | 0.36 | 0.45 | 0.58 | 0.58 | 30.11 | 20 | 0 | 0.000 | 0 |
| cold-object-gist-multirange | 977.57 | 0.42 | 4.20 | 8.43 | 8.43 | 30.00 | 20 | 0 | 0.150 | 0 |
| cold-object-gist-network | 1066.29 | 0.35 | 3.94 | 8.00 | 8.00 | 30.03 | 20 | 0 | 0.150 | 0 |
| cold-object-gist-spatial | 1025.74 | 0.58 | 0.91 | 8.05 | 8.05 | 30.11 | 20 | 0 | 0.100 | 0 |
| cold-object-gist-tsvector | 163.62 | 3.47 | 7.03 | 69.44 | 69.44 | 30.33 | 20 | 0 | 0.700 | 0 |
| cold-object-join-probe | 67.68 | 7.04 | 15.46 | 152.12 | 152.12 | 260.61 | 1842 | 0 | 0.900 | 0 |
| cold-object-ordered-limit | 213.76 | 4.23 | 7.28 | 10.38 | 10.38 | 159.14 | 20 | 0 | 0.000 | 0 |
| cold-object-point | 4904.19 | 0.68 | 1.55 | 2.18 | 2.18 | 27.27 | 80 | 0 | 0.000 | 0 |
| cold-object-spgist-knn | 1748.76 | 0.34 | 0.56 | 4.28 | 4.28 | 30.58 | 20 | 0 | 0.050 | 0 |
| cold-object-spgist-network | 1126.57 | 0.30 | 3.82 | 7.58 | 7.58 | 30.56 | 20 | 0 | 0.150 | 0 |
| cold-object-spgist-prefix | 910.61 | 0.27 | 1.20 | 14.90 | 14.90 | 30.47 | 20 | 0 | 0.100 | 0 |
| cold-object-spgist-range | 1089.77 | 0.35 | 4.01 | 7.17 | 7.17 | 30.55 | 20 | 0 | 0.150 | 0 |
| cold-object-spgist-spatial | 360.09 | 0.55 | 11.74 | 25.31 | 25.31 | 30.56 | 30 | 0 | 0.600 | 0 |
| cold-object-tail-range | 29.86 | 21.69 | 49.34 | 199.23 | 199.23 | 264.42 | 58 | 0 | 1.000 | 0 |
| concurrent-insert | 101.47 | 39.37 | 41.34 | 42.09 | 42.09 | 273.98 | 0 | 0 | 1.500 | 0 |
| concurrent-update | 93.66 | 38.91 | 52.03 | 82.84 | 82.84 | 273.73 | 80 | 0 | 1.500 | 0 |
| mixed-baseline | 405.83 | 0.68 | 35.57 | 37.91 | 37.91 | 274.00 | 80 | 0 | 0.412 | 0 |
| mixed-checkpoint-interference | 15.94 | 17.33 | 991.69 | 1756.83 | 1756.83 | 282.55 | 130 | 0 | 17.050 | 0 |
| point-concurrency-1 | 21.39 | 52.60 | 66.34 | 121.05 | 121.05 | 272.00 | 282 | 0 | 13.100 | 0 |
| postgresql18-insert | 3255.10 | 1.23 | 2.98 | 4.33 | 4.33 | — | 0 | 0 | — | 0 |
| postgresql18-join-probe | 14916.45 | 0.20 | 0.35 | 1.40 | 1.40 | — | 0 | 80 | — | 0 |
| postgresql18-mixed | 7359.76 | 0.15 | 1.37 | 4.77 | 4.77 | — | 80 | 0 | — | 0 |
| postgresql18-ordered-limit | 2073.06 | 1.91 | 2.57 | 3.54 | 3.54 | — | 80 | 0 | — | 0 |
| postgresql18-point-concurrency-1 | 8178.15 | 0.08 | 0.13 | 0.75 | 0.75 | — | 20 | 0 | — | 0 |
| postgresql18-point | 13557.12 | 0.23 | 0.48 | 1.38 | 1.38 | — | 80 | 0 | — | 0 |
| postgresql18-scan | 2284.84 | 0.09 | 0.74 | 0.74 | 0.74 | — | 0 | 2 | — | 0 |
| postgresql18-tail-range | 11110.27 | 0.21 | 0.39 | 3.03 | 3.03 | — | 80 | 0 | — | 0 |
| warm-disk-point | 653.43 | 1.43 | 11.37 | 60.41 | 60.41 | 138.52 | 110 | 0 | 0.263 | 0 |
| warm-memory-brin-inclusion | 4087.64 | 0.92 | 1.26 | 1.92 | 1.92 | 272.41 | 80 | 0 | 0.000 | 0 |
| warm-memory-brin-point | 5110.84 | 0.67 | 1.49 | 1.77 | 1.77 | 272.33 | 80 | 0 | 0.000 | 0 |
| warm-memory-gin-array-overlap | 7849.29 | 0.49 | 0.57 | 0.99 | 0.99 | 272.72 | 80 | 0 | 0.000 | 0 |
| warm-memory-gin-array | 7221.57 | 0.52 | 0.77 | 1.11 | 1.11 | 272.72 | 80 | 0 | 0.000 | 0 |
| warm-memory-gin-jsonb-path | 4516.44 | 0.77 | 1.46 | 2.25 | 2.25 | 272.92 | 80 | 0 | 0.000 | 0 |
| warm-memory-gin-jsonb | 6419.69 | 0.55 | 0.91 | 1.50 | 1.50 | 272.78 | 80 | 0 | 0.000 | 0 |
| warm-memory-gin-tsvector | 5438.77 | 0.68 | 0.99 | 1.22 | 1.22 | 272.73 | 80 | 0 | 0.000 | 0 |
| warm-memory-gist-inclusion | 4142.16 | 0.87 | 1.38 | 2.43 | 2.43 | 272.56 | 80 | 0 | 0.000 | 0 |
| warm-memory-gist-knn | 2927.37 | 1.19 | 2.26 | 4.70 | 4.70 | 272.72 | 80 | 0 | 0.000 | 0 |
| warm-memory-gist-multirange | 3606.22 | 1.07 | 1.51 | 2.22 | 2.22 | 272.58 | 80 | 0 | 0.000 | 0 |
| warm-memory-gist-network | 5772.23 | 0.67 | 0.85 | 1.18 | 1.18 | 272.58 | 80 | 0 | 0.000 | 0 |
| warm-memory-gist-spatial | 2569.10 | 1.52 | 1.92 | 2.12 | 2.12 | 272.59 | 80 | 0 | 0.000 | 0 |
| warm-memory-gist-tsvector | 3357.78 | 1.13 | 1.52 | 1.92 | 1.92 | 272.73 | 80 | 0 | 0.000 | 0 |
| warm-memory-join-probe | 157.50 | 25.27 | 26.39 | 26.89 | 26.89 | 273.52 | 5120 | 0 | 0.000 | 0 |
| warm-memory-ordered-limit | 2435.14 | 1.46 | 3.02 | 3.66 | 3.66 | 273.48 | 80 | 0 | 0.000 | 0 |
| warm-memory-point | 699.32 | 0.49 | 49.55 | 54.33 | 54.33 | 272.17 | 155 | 0 | 0.375 | 0 |
| warm-memory-spgist-knn | 3289.63 | 1.13 | 1.68 | 2.84 | 2.84 | 273.20 | 80 | 0 | 0.000 | 0 |
| warm-memory-spgist-network | 5281.41 | 0.70 | 1.08 | 1.88 | 1.88 | 273.20 | 80 | 0 | 0.000 | 0 |
| warm-memory-spgist-prefix | 2610.93 | 0.74 | 4.72 | 9.09 | 9.09 | 272.95 | 80 | 0 | 0.000 | 0 |
| warm-memory-spgist-range | 3836.45 | 1.00 | 1.45 | 1.64 | 1.64 | 273.16 | 80 | 0 | 0.000 | 0 |
| warm-memory-spgist-spatial | 2142.13 | 1.83 | 2.47 | 2.62 | 2.62 | 273.20 | 80 | 0 | 0.000 | 0 |
| warm-memory-tail-range | 168.09 | 23.39 | 24.31 | 29.74 | 29.74 | 273.42 | 160 | 0 | 0.000 | 0 |

## Recovery and cache-fill intervals

| Cache state | startup s | GET | ranged GET | LIST | response MiB |
|---|---:|---:|---:|---:|---:|
| cold-object-join-recovery | 0.062 | 5 | 11 | 0 | 0.027 |
| cold-object-recovery | 0.029 | 5 | 21 | 0 | 0.147 |
| initial-start | 0.352 | 0 | 0 | 1 | 0.000 |
| warm-disk-recovery | 0.036 | 7 | 26 | 0 | 0.153 |

## Derived comparisons

- warm-disk / warm-memory p99: 1.11x
- cold-object / warm-memory p99: 0.04x
- checkpoint-overlap / baseline p99: 46.34x
- pos3ql / PostgreSQL 18 warm point throughput: 0.05x
- pos3ql concurrent / single-client point throughput: 32.69x
- PostgreSQL 18 concurrent / single-client point throughput: 1.66x
- pos3ql / PostgreSQL 18 insert throughput: 0.03x
- pos3ql / PostgreSQL 18 scan throughput: 0.05x

## Recorded database identities

- `PostgreSQL 18.4 (pos3ql 0.1.0) on aarch64-apple-darwin`
- `PostgreSQL 18.6 (Homebrew) on aarch64-apple-darwin24.6.0, compiled by Apple clang version 17.0.0 (clang-1700.6.4.2), 64-bit`
