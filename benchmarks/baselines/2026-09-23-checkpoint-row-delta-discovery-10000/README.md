# Exact row-delta discovery, 10,000 rows

The [environment manifest](environment.json), [PostgreSQL settings](postgresql-server.json),
[raw mixed workload](mixed-checkpoint-interference.json), [checkpoint phase events](checkpoint-profile.json),
[server log](pos3ql-startup.log), and [derived report](report.md) preserve a
run from clean implementation commit `b55e5b148ca9fb67afce01c1694a0b3f49d6a8bd`.
The release binary SHA-256 is
`779e19713c2a5ea0b8c7881b35b895fbd5a9f5d9ef92c12cb82b41d19c6c150c`.
The manifest records the host, toolchain, 2 GiB fixed disk cache, two-millisecond
injected object latency, eight-second minimum workload duration, 300-second
query timeout, and settling checkpoint before the interference window.

Each committed row now retains the LSN of its latest change until a manifest
publishes the matching table generation. That fixed-capacity overlay metadata
keeps the exact unpublished row identities available even when row bytes spill.
An ordinary row delta enumerates only those identities, while a full rewrite
still walks the complete logical table. Compatible reslices use their prior
slice LSN as the boundary. Ordinary deltas continue to repeat resident
historical images required by pair-merge snapshot pruning; spilled history is
already present in the immutable base.

Compared with the preceding [incremental value-index profile](../2026-09-23-checkpoint-value-index-delta-10000/README.md),
the row-delta phases recorded:

| Row-delta measure | Preceding run | This run |
|---|---:|---:|
| events | 2 | 2 |
| block GET | 228 | 0 |
| block PUT | 8 | 8 |
| phase time | 734.49 ms | 33.34 ms |
| largest span | 702.96 ms | 19.84 ms |
| largest event GET | 227 | 0 |
| largest event PUT | 4 | 4 |

A separate cache-disabled regression removes all redundant resident rows after
the base checkpoint and requires both an initial 128-row delta and a later
one-row reslice to make zero object GETs. It verifies the resulting rows before
and after object-cold recovery. A 24-generation repeatable-read regression
keeps an old snapshot pinned across row merges, updates, and a delete. The
complete storage VOPR range, seeds 460259 through 460274, covers publication
failure, outage, corruption, cold restart, and warm restart schedules. WAL
replay now reports `PROGRAM_LIMIT_EXCEEDED` if a spilled-row delete cannot
retain its tombstone identity in the configured `table_rows` overlay.

Other phase totals are not a controlled comparison. This run's row-merge
schedule made 216 GETs over 62 bounded events, versus 47 over 42 events in the
preceding run; no event exceeded eight GETs. Row-merge writing made 74 PUTs
over 233 events, and no event exceeded five PUTs. Shared-host scheduling also
produced one 331.82 ms write event despite zero GETs. These differences show
that aggregate phase time and traffic depend on the generation shape admitted
by the duration floor.

Checkpoint-pressure throughput was 208.87 operations per second, p99 was
69.34 ms, and maximum latency was 869.44 ms. Actual PostgreSQL 18.6 completed
the matched workload at 5,037.17 operations per second, 4.59 ms p99, and
27.87 ms maximum latency. PostgreSQL ran unmodified with `fsync`,
`full_page_writes`, and `synchronous_commit` enabled on its recorded
Docker-managed local volume. pos3ql used an instrumented S3-compatible fixture
backed by temporary local storage. These persistence tiers differ, and
PostgreSQL has no equivalent object-request metric.

Reproduce with Docker available for the actual PostgreSQL 18 comparison:

```sh
POS3QL_BENCH_ROWS=10000 POS3QL_BENCH_TABLE_CAPACITY=16384 \
POS3QL_BENCH_OPERATIONS=100 POS3QL_BENCH_CLIENTS=4 \
POS3QL_BENCH_REPLICAS=0 POS3QL_BENCH_DISK_CACHE_MIB=2048 \
POS3QL_BENCH_TIMEOUT_SECONDS=300 POS3QL_BENCH_CHECKPOINT_SECONDS=8 \
POS3QL_BENCH_CHECKPOINT_PROFILE=1 \
tools/run-performance.sh checkpoint ./performance-results/checkpoint-row-delta-discovery-10000
```
