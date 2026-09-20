# Focused checkpoint sort-run run, 1,000 rows

The [environment manifest](environment.json), [raw mixed workload](mixed-checkpoint-interference.json),
[setup workload](point-concurrency-1.json), [startup log](pos3ql-startup.log), and
[derived report](report.md) come from clean commit
`7bb1dcaf12f981afdedc9a803063e61ecc0d6d08`. The manifest records the
release binary SHA-256, host, toolchain, cache, and timeout.

The same four-client, 1,000-row focused workload as the
[previous clean run](../2026-09-20-checkpoint-block-reuse-1000/README.md)
completed 200 mixed operations and three explicit checkpoints without errors.
Object PUTs fell from 1,556 to 814, and DELETEs from 1,071 to 553. These
whole-workload counts include foreground traffic and cleanup. A zero-cache
one-row-update regression isolates the checkpoint: block PUTs fell from 19 to
11, with the same indexed results after object-cold recovery. The sorter now
keeps complete small runs in its startup buffer; rows beyond that buffer still
use the provider-neutral external merge.

Both runs used an Apple Silicon macOS 15.6.1 host with 12 logical CPUs and
24 GiB RAM, plus the instrumented S3-compatible fixture backed by temporary
local storage with 2 ms injected request latency. This run completed in 6.96
seconds with 1,259.12 ms p99, versus 12.67 seconds and 1,929.52 ms in the
previous run. These are single runs on a shared host, so the timing pair does
not qualify a production speedup. PostgreSQL 18 on local APFS remains the
[separate SQL comparison baseline](../2026-09-20-postgresql18-local-apfs-1000/README.md).

Reproduce the focused run with:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-sort-runs-1000
```
