# Exploratory PostgreSQL 18 comparison, 1,000 rows

This directory preserves the complete schema-v1 output and derived
[`report.md`](report.md) from `tools/run-performance.sh full`. All 59 workloads
completed without errors. The pos3ql binary came from clean commit
`d1aba82ea63a83ff9eed8febd9ca247691ca666f`; [`environment.json`](environment.json)
records its SHA-256, toolchain, host, workload shape, cache size, and timeout.

The host was an Apple Silicon macOS 15.6.1 machine with 12 logical CPUs and
24 GiB RAM. pos3ql used the instrumented S3-compatible test server backed by
temporary local storage, with 2 ms injected request latency. PostgreSQL 18.6
was an unmodified Homebrew build with an isolated temporary data directory on
local APFS (`/dev/disk3s5`). Its `fsync`, `full_page_writes`, and
`synchronous_commit` settings were on; the captured settings are in
[`postgresql-server.json`](postgresql-server.json). Both servers shared this
unreserved host. The PostgreSQL data directory was removed after the run.

The suite used 1,000 setup rows, capacity for 2,048 rows, four clients, 50
operations per client, and no logical replicas. pos3ql had a 32 MiB RAM block
cache and a 1 GiB fixed disk block cache. Each timed query had a 120-second
socket timeout. PostgreSQL was launched with `initdb -U postgres -A trust
--encoding=UTF8 --lc-collate=C --lc-ctype=C` and `pg_ctl` options
`-c listen_addresses=127.0.0.1 -c fsync=on -c full_page_writes=on
-c synchronous_commit=on`. With that server on a free port, the invocation was:

```sh
POS3QL_BENCH_ROWS=1000 \
POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 \
POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 \
POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 \
POS3QL_BENCH_POSTGRES_PORT="$PG_PORT" \
POS3QL_BENCH_POSTGRES_STORAGE='Local APFS volume /System/Volumes/Data (/tmp), PostgreSQL data directory on /dev/disk3s5; fsync enabled' \
tools/run-performance.sh full ./performance-results/local-postgresql18-1000
```

The same wire client issued the same SQL and operation counts to both engines
for the paired scenarios. Selected values from the raw files:

| Scenario | pos3ql ops/s | PostgreSQL ops/s | pos3ql p95 ms | PostgreSQL p95 ms |
|---|---:|---:|---:|---:|
| Four-client point reads | 935 | 16,185 | 46.88 | 0.37 |
| Four-client inserts | 105 | 9,856 | 41.33 | 1.13 |
| Three full-table aggregates | 56 | 2,396 | 42.17 | 0.82 |

The same-process point workload still made 0.48 object requests per operation;
its `warm-memory` label does not imply a fully resident RAM run. The later
`cold-object-point` workload made no object requests because preceding cold
workloads had warmed the new local caches. Those labels describe suite order,
not isolated cache-state experiments.

The pos3ql mixed workload without explicit maintenance completed 200 calls at
400 ops/s with p99 of 39.50 ms. With three explicit checkpoints, it completed
the same 200 calls at 8.45 ops/s with p99 of 4,355.29 ms and 5,902 object
requests, including 3,466 PUTs and 1,812 DELETEs. This is a material
checkpoint-interference cost to investigate. These runs are too short and the
host too shared for production ratios; CPU counters were unavailable on macOS
in this harness. Peak RSS, fixed-memory occupancy, object bytes and requests,
recovery intervals, and access-path counters remain in the raw artifacts.

The earlier [256-row run](../2026-09-20-postgresql18-local-apfs/README.md)
used a 128 MiB disk cache and a 30-second query timeout. Its incomplete
1,000-row attempt exposed an object-read slot liveness defect. This run uses
the slot and spilled-scan fixes, so cross-run differences cannot be attributed
to row count alone. Further qualification needs isolated cache-state runs,
larger datasets, logical replicas, pinned hardware, and an independently
operated compatible object store.
