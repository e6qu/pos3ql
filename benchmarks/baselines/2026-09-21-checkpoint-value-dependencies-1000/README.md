# Selective value-index publication, 1,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean commit `9207132c471729b77243f579bad867179494afd0`. The
manifest records the release binary SHA-256, host, toolchain, cache, four-second
minimum workload duration, 120-second query timeout, and settling checkpoint
before the interference window.

Each durable value-index binding now records the physical table columns that
can affect its key, partial predicate, or included payload. The precommit row
path compares canonical encoded column payloads without allocation and marks
only intersecting bindings dirty. Insert and delete still invalidate every
binding because even a constant expression or predicate changes membership.
DDL, row rewrites, REINDEX, unlogged reset, and WAL replay use a conservative
all-binding invalidation boundary. This lets a checkpoint carry unrelated
immutable index generations forward without weakening recovery.

A regression covers primary, covering, expression, partial, and GIN indexes.
An unrelated-column update retains every handle; an included-column update
invalidates only the covering and partial-expression bindings. A WAL-only
insert is then recovered, checkpointed, and recovered again to prove that
replay invalidation publishes both B-tree and GIN entries. The new masks and
dirty state are charged to startup memory; the exact wide-catalog budget test
was raised by 1 MiB to include them.

The earlier [final-slice run](../2026-09-21-checkpoint-final-slice-1000/README.md)
has the same settled shape of three row/value publication events and one
metadata-only shutdown manifest. Its three value-index phases wrote 224 blocks
and occupied 1,034.94 ms. This run wrote 24 blocks in three value-index phases
and occupied 192.69 ms. Row SST phases wrote 105 blocks versus 117, and the
full request window recorded 576 PUTs versus 765. These shared-host runs are
exploratory, but the matched event shape and phase attribution isolate the
intended reduction in value-index publication traffic.

All 866 pos3ql foreground operations and three explicit checkpoints completed.
Its p99 was 80.10 ms without explicit checkpoints and 328.69 ms with them.
Actual PostgreSQL 18.6 completed 10,485 operations and three explicit
checkpoints in its matched interference workload, with p99 of 11.06 ms in the
baseline and 9.81 ms under checkpoint pressure. PostgreSQL ran unmodified in
the recorded Docker image with `fsync`, `full_page_writes`, and
`synchronous_commit` enabled on a Docker-managed local volume. pos3ql used a
1 GiB fixed disk cache and an instrumented S3-compatible fixture backed by
temporary local storage with 2 ms injected request latency. PostgreSQL's local
durable tier has no equivalent object-request metric.

Reproduce with Docker available for the actual PostgreSQL 18 comparison:

```sh
POS3QL_BENCH_ROWS=1000 POS3QL_BENCH_TABLE_CAPACITY=2048 \
POS3QL_BENCH_OPERATIONS=50 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=1024 \
POS3QL_BENCH_TIMEOUT_SECONDS=120 POS3QL_BENCH_CHECKPOINT_SECONDS=4 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 \
POS3QL_BENCH_POSTGRES_STORAGE='Docker-managed local volume on host APFS; fsync enabled' \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-value-dependencies-1000
```
