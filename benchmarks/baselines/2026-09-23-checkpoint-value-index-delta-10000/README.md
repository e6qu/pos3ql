# Incremental value-index checkpoint merge, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean implementation commit `9ca60536fa22b565b65a255756ac75cd61bf3c90`.
The release binary SHA-256 is
`cdb5c190daebf7903d0bf813e6431dea8efe50d09340651564e0dbf970cb4e80`.
The manifest records the host, toolchain, 2 GiB fixed disk cache, two-millisecond
injected object latency, eight-second minimum workload duration, 300-second
query timeout, and settling checkpoint before the interference window.

An ordinary committed change now contributes only rows newer than the
published value-index LSN to the fixed-memory sorter. The writer merges that
delta with a restartable ordered stream of the prior ordinary roster or
navigation tree. Every prior entry for a changed row identity is suppressed,
so key changes, predicate exits, deletes, included payload changes, and GIN
posting changes leave one exact replacement generation. Content-addressed
data and navigation blocks remain reusable when their bytes are unchanged.
Relation rewrites, catalog changes, and `REINDEX` explicitly retain the full
source walk. Object failures restart both inputs from their immutable roots.

Compared with the preceding [immutable PAX group reuse profile](../2026-09-22-checkpoint-row-merge-reuse-10000/README.md),
the value-index phases recorded:

| Value-index measure | Preceding run | This run |
|---|---:|---:|
| schedule events | 398 | 12 |
| write events | 62 | 64 |
| combined block GET | 910 | 0 |
| combined block PUT | 204 | 66 |
| combined phase time | 4,050.12 ms | 376.78 ms |
| largest schedule span | 37.88 ms | 0.74 ms |
| largest write span | 15.35 ms | 33.64 ms |

All value-index beats stayed within the four-PUT and eight-GET boundaries.
Schedule became CPU-only because the changed rows remained in the bounded
resident overlay, while base blocks hit the fixed disk cache. A separate
cache-disabled navigation regression bounds the provider path at 24 total GETs
and 12 PUTs, changes both an indexed key and included payload, runs garbage
collection, and verifies the exact nearest-neighbor result after object-cold
recovery. The existing fault regression invalidates a staged generation after
an interleaved commit and restarts after a failed remote write.

Correctness validation rejected a row-SST optimization attempted during this
audit. A changed row can spill before checkpoint, so enumerating only the
resident map lost that row after cold recovery. The final implementation keeps
the complete logical row scan, and deterministic storage VOPR seed 460260
reproduces the case. In this run, two `row_sst_delta` events made 228 GETs and
eight PUTs over 751.76 ms; the largest event made 227 GETs over 719.40 ms. This
is the next measured checkpoint construction target.

The duration floor admitted 1,485 foreground operations, versus 825 in the
preceding run, so aggregate request totals and latency are not a controlled
ratio. Checkpoint-pressure throughput was 184.57 operations per second, p99
was 111.53 ms, and maximum latency was 1,123.99 ms. Actual PostgreSQL 18.6
completed the matched workload at 3,043.16 operations per second, 7.07 ms p99,
and 41.20 ms maximum latency. PostgreSQL ran unmodified with `fsync`,
`full_page_writes`, and `synchronous_commit` enabled on its recorded
Docker-managed local volume. pos3ql used an instrumented S3-compatible fixture
backed by temporary local storage. These persistence tiers differ, and
PostgreSQL has no equivalent object-request metric.

No value-index construction event exceeded the established per-beat request
limits. Row merge scheduling made 41 GETs over 121.72 ms, and row merge writing
made 86 PUTs over 1,009.12 ms. Their largest events were 23.73 and 22.66 ms.
Post-publication commit pruning made 302 deletes over 1,203.34 ms.

Reproduce with Docker available for the actual PostgreSQL 18 comparison:

```sh
POS3QL_BENCH_ROWS=10000 POS3QL_BENCH_TABLE_CAPACITY=16384 \
POS3QL_BENCH_OPERATIONS=100 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=2048 \
POS3QL_BENCH_TIMEOUT_SECONDS=300 POS3QL_BENCH_CHECKPOINT_SECONDS=8 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-value-index-delta-10000
```
