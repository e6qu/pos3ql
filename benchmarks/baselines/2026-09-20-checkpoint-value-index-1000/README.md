# Focused checkpoint run, 1,000 rows

The [environment manifest](environment.json), [raw workload](mixed-checkpoint-interference.json),
[setup workload](point-concurrency-1.json), [startup log](pos3ql-startup.log),
and [derived report](report.md) come from clean commit
`9282b6b50a108dfb910717a9a371027a2ea09e14`. The manifest records the
release binary SHA-256, host, toolchain, cache, and workload settings.

The run used an Apple Silicon macOS 15.6.1 host with 12 logical CPUs and
24 GiB RAM. The instrumented S3-compatible fixture used temporary local
storage with 2 ms injected request latency. The four clients completed
200 mixed operations with three explicit checkpoints and no errors in
19.39 seconds; p99 latency was 2,217.77 ms. This one local run is
exploratory. Its request totals also include publication and garbage
collection, so they do not isolate the value-index read reduction.

Reproduce the focused run with:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-1000
```

The earlier [full comparison](../2026-09-20-postgresql18-local-apfs-1000/README.md)
uses actual PostgreSQL 18 on local APFS. That suite includes other workloads
before its mixed checkpoint scenario, so the two timing results do not form a
controlled before-and-after pair. The cacheless checkpoint regression in
`src/sql/tests.rs` isolates the repeated object reads separately.
