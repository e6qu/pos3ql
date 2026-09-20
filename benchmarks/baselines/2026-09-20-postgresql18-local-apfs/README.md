# Exploratory PostgreSQL 18 baseline, 2026-09-20

This directory preserves the complete schema-v1 JSON output and derived
[`report.md`](report.md) from a full `tools/run-performance.sh` run. All 59
workloads completed without errors. The pos3ql binary was built from
`1ef10592879fd403b8c036a1fcb5c43afb82ea7f` with a clean worktree;
[`environment.json`](environment.json) records its SHA-256, toolchain, host,
and workload shape.

The host was an Apple Silicon macOS 15.6.1 machine with 12 logical CPUs and
24 GiB RAM. pos3ql used the instrumented S3-compatible test server, with
temporary local filesystem backing and 2 ms injected request latency.
PostgreSQL 18.6 was an unmodified Homebrew build with an isolated temporary
data directory on local APFS (`/dev/disk3s5`); `fsync`, `full_page_writes`, and
`synchronous_commit` were on. Its exact settings are in
[`postgresql-server.json`](postgresql-server.json). The PostgreSQL data
directory was deleted after the run. Both servers shared this host, which was
not reserved for benchmarking.

The full suite used 256 setup rows, capacity for 512 rows, four clients,
20 operations per client, and zero logical replicas. PostgreSQL was launched
with `initdb -U postgres -A trust --encoding=UTF8 --lc-collate=C --lc-ctype=C`
and `pg_ctl` options `-c listen_addresses=127.0.0.1 -c fsync=on
-c full_page_writes=on -c synchronous_commit=on`. With that server listening
on a free port, the suite invocation was:

```sh
POS3QL_BENCH_ROWS=256 \
POS3QL_BENCH_TABLE_CAPACITY=512 \
POS3QL_BENCH_OPERATIONS=20 \
POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 \
POS3QL_BENCH_POSTGRES_PORT="$PG_PORT" \
POS3QL_BENCH_POSTGRES_STORAGE='Local APFS volume /System/Volumes/Data (/tmp), PostgreSQL data directory on /dev/disk3s5; fsync enabled' \
tools/run-performance.sh full ./performance-results/local-postgresql18
```

For the paired scenarios, the same wire client issued the same SQL and
operation counts to each engine.
Selected observations from the raw files:

| Scenario | pos3ql ops/s | PostgreSQL ops/s | pos3ql p95 ms | PostgreSQL p95 ms |
|---|---:|---:|---:|---:|
| Four-client point reads after setup | 699 | 13,557 | 49.55 | 0.48 |
| Four-client inserts | 101 | 3,255 | 41.34 | 2.98 |

The pos3ql point run made 0.375 object requests per completed operation, so
the `warm-memory-point` label describes the same-process run after setup, not
a fully resident RAM run. The corresponding PostgreSQL run recorded 80 index
scans and no sequential scans. The pos3ql insert run made 1.5 object requests
per operation; the PostgreSQL run used its ordinary local WAL durability path.
The checkpoint-overlap run completed 80 operations and three explicit
checkpoints, with p99 of 1,756.83 ms versus 37.91 ms for the separate mixed
run without explicit checkpoints. These short runs show costs to investigate,
not stable production ratios. CPU counters were unavailable on macOS in this
harness; peak RSS and object-request counts remain in the raw artifacts.

An earlier, incomplete 1,000-row attempt is preserved in
[`attempt-1000`](attempt-1000). Its SP-GiST text-prefix case completed zero of
200 calls before the 30-second client timeout. It did not reach PostgreSQL,
so it supplies no larger-scale comparison. The attempted run used an
uncommitted harness revision; its environment manifest records that fact.
The next measurement needs a settled warm cache, more rows and operations,
logical replicas, pinned host load, and an independently operated compatible
object store. The prefix timeout needs diagnosis before that longer suite can
complete.
