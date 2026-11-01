# Share fixed capacity according to demand

**Design goal; unfinished.** pos3ql must keep its startup allocations and arenas,
while assigning their capacity according to current work. This page describes
the proposed policy and its evidence. [PLAN.md](../PLAN.md) owns the remaining
work and sequence; [architecture](architecture.md) owns current behavior.

For example, reserving space for many possible tables should not give every
empty table tracking space for its maximum possible row count. If most requests touch `orders`, that
table should borrow available shared capacity. When a report starts reading
`sales`, capacity that is safe to reuse should follow the new demand. The total
reserved memory must remain unchanged.

## What already exists

- [Storage startup accounting](../src/storage/mod.rs#L19671) still charges
  `max_tables * RowMap::budget_bytes(table_rows)` for row lookup structures.
  Table construction creates
  those maps before serving requests. Removing this multiplication requires
  shared ownership and admission, not merely changing a configuration label.
- [Row bytes](../src/storage/row_heap.rs) already share two `memtable_bytes`
  buffers for active data and relocation. Committed MVCC history uses the
  instance-wide `row_history_slots`; locking mode reserves no committed history.
- [Copies of table definitions](../src/storage/definition_images.rs) are already
  leased globally to readers, but reserve `(max_tables + 1) *
  (max_catalog_versions_per_object + 1) * query_workspace_slots` wide slots.
  [Retained row walks](../src/storage/row_walks.rs) reserve
  `query_workspace_slots * 64` identities, each with
  `max(table_rows, large_object_pages, value_index_rows)` row-ID positions.
  Reducing per-table maps alone would leave these multipliers in place.
- The [RAM block cache](../src/store/cache.rs) and
  [disk block cache](../src/store/disk.rs) already share fixed slots across
  objects. CLOCK eviction gives recently used blocks another chance to remain.
  The disk file has a fixed logical length; sizing it does not guarantee
  physical disk space has been reserved.
- [SST writing](../src/store/sst.rs) uses compressed row groups for small deltas
  and PAX, column-oriented row groups, for full slices. The
  [checkpoint writer](../src/checkpoint.rs#L13786) selects these formats.
  [Column reading](../src/storage/mod.rs#L32209) retains completed columns and
  starts missing selected columns before yielding. Some paths reuse a whole
  packed container instead of fetching its extents separately.
- [Read-ahead](../src/store/sst.rs#L2525) uses the configured request slots and
  leaves one for the current read. That window skips packed references.
  [Physical output batches](../src/store/write_batch.rs) fill available upload
  slots. [Group commit](../src/server.rs#L1758) shares publication work from one
  reactor turn without adding a timer to collect more arrivals.

These mechanisms do not establish workload-driven resource allocation or
production performance. Existing [measurements](performance.md) retain that
distinction.

## Keep capacity fixed; change who can use it

Allocate shared backing pools, their indexes and controller state at startup.
A lease is temporary ownership of a portion of that reserved space. Tables,
queries and maintenance acquire and release leases without growing the heap.
Keep small table identities separate from expensive resident row capacity.

Classify capacity before sharing it. A verified cache block can be evicted;
uncommitted changes, a reader's selected version and an unfinished upload
cannot. Reassign only released capacity. Retain stable identities and ownership
through rollback, saved transactions, table replacement and cold recovery.
Use bounded reclamation and explicit backpressure when all suitable capacity
is busy. Report named exhaustion instead of silently growing a pool.

Memory and disk need separate limits. Sharing cache slots must not consume
journal or session-temporary storage capacity. If startup must guarantee
physical disk reservation, implement and verify that guarantee explicitly;
file length alone is insufficient. Object storage remains the durable tier.

## Follow the workload without changing SQL behavior

| User workload | Intended response within the fixed budgets |
|---|---|
| Repeated single-order lookups | Retain useful row/index blocks and issue demand reads promptly |
| A report selecting two columns from a wide table | Fetch needed column groups; keep a large scan from evicting every frequently reused block |
| Many small order updates | Give active write buffers capacity and drain immutable output without starving reads |
| Sustained bulk ingestion | Group compatible queued output and overlap uploads; preserve commit order and durability |
| Traffic moves to a different table | Release unused leases and move reusable capacity after bounded observation |
| A long report keeps old versions | Keep those versions owned; expose the resulting pressure instead of reclaiming them |

Choose whole-container versus ranged reads using selected bytes, reusable
contents, request overhead and observed transfer cost. Make future row/column
layout decisions more slowly than request scheduling. Account for rewrite
costs and retained readers; avoid rewriting stored data after every short burst.
Adaptation must preserve declared SQL types, isolation, errors and once-only
effects. It must not turn a selected-column query into an unchecked answer.

## Batch for throughput; dispatch promptly when idle

An isolated demand read or commit should start without waiting to fill a batch.
With sustained arrivals, group already queued compatible work until the byte
bound or oldest request's collection deadline is reached. Reserve progress for
demand reads, commit publication and required maintenance. Speculative reads
and duplicate hedged requests must yield before essential work is starved.

Expose an explicit fixed or adaptive policy, hard capacities, minimum progress
guarantees, maximum batch bytes, maximum collection delay, observation interval
and minimum sample count. These are proposed controls, not available settings.
Show current active limits and why they changed. A latency target guides policy;
it does not guarantee completion during an object-store outage.

Compare a simple bounded feedback controller with proportional-integral or
proportional-integral-derivative control, called PI or PID. Do not choose PID
merely because it is more elaborate. Delayed completions, differing object sizes
and discrete leases require controlled experiments. Clamp decisions to actual
capacity, limit changes per interval and require sufficient samples. Add
hysteresis, a range that avoids repeated reversals, and minimum time between
changes. If integral control is used, prevent accumulated error from demanding
unavailable capacity; this is anti-windup. Keep idle periods and failed requests
from falsely appearing to be successful low-latency samples.

## Evidence required before calling this implemented

Measure queue time separately from object service time, useful and transferred
bytes, active/waiting leases, pinned bytes, cache reuse/eviction, wasted
read-ahead, batch fill, maintenance backlog and controller decisions. Existing
[block counters](../src/store/mod.rs#L541) are a starting point; sampling buffers
and latency distributions must also fit startup budgets.

Test idle, burst, sustained and cooling traffic; switches between hot tables;
point reads mixed with scans; narrow-column reports; small updates and bulk
writes. Inject bandwidth changes, delayed/failed responses, cancellation and
long-lived readers. Prove fixed memory/file bounds, no runtime allocation,
safe ownership, fair progress and unchanged recovery and transaction results.

Compare adaptive and explicit fixed policies at equal resource limits on MinIO
and SeaweedFS. Retain both vanilla PostgreSQL controls described in
[performance](performance.md). Publish raw latency distributions, throughput,
requests, bytes and resource usage, including regressions and settling after a
load change. Select policy and numerical targets from that evidence.

## Design references

- [Envoy adaptive concurrency](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/adaptive_concurrency_filter.html)
  adjusts outstanding request limits using sampled latency and requires control
  over the relevant concurrency. This is a design reference, not pos3ql code.
- [Netflix concurrency-limits](https://github.com/Netflix/concurrency-limits)
  describes Vegas and Gradient2 feedback and partitioned concurrency guarantees.
- [Åström and Murray, PID control](https://www.cds.caltech.edu/~murray/books/AM05/pdf/fbs-pid_01Jan19.pdf)
  explains filtering, saturation and anti-windup. Applying these methods here
  remains a proposal requiring the experiments above.
