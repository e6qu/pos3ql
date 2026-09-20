# Focused checkpoint block-reuse run, 1,000 rows

The [environment manifest](environment.json), [raw mixed workload](mixed-checkpoint-interference.json),
[setup workload](point-concurrency-1.json), [startup log](pos3ql-startup.log), and
[derived report](report.md) come from clean commit
`57bd13623750b4b53721589b5df09c0c81aa0c48`. The manifest records the
release binary SHA-256, host, toolchain, cache, and timeout.

The same four-client, 1,000-row focused workload as the
[previous clean run](../2026-09-20-checkpoint-value-index-1000/README.md)
completed 200 mixed operations and three explicit checkpoints with no errors.
This run made 1,556 object PUTs, versus 2,595 in the previous run. The
40% reduction includes all workload requests, not only index writes. The
zero-cache regression `checkpoint_reuses_published_index_blocks_after_small_update`
isolates one updated table: 30 block PUTs on merged main `c80eca72`, 19 with
reuse, and the same indexed result after object-cold recovery.

Both runs used an Apple Silicon macOS 15.6.1 host with 12 logical CPUs and
24 GiB RAM, plus the instrumented S3-compatible fixture backed by temporary
local storage with 2 ms injected request latency. This run completed in
12.67 seconds with p99 latency of 1,929.52 ms; the previous run took 19.39
seconds with 2,217.77 ms p99. These are single runs on a shared host.
Garbage DELETE counts also changed from 970 to 1,071, so the timing pair
does not isolate a complete checkpoint speedup. PostgreSQL 18 on local APFS
remains the [separate SQL comparison baseline](../2026-09-20-postgresql18-local-apfs-1000/README.md).

Reproduce the focused run with:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-block-reuse-1000
```
