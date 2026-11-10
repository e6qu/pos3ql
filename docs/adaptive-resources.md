# Share fixed capacity according to demand

This is the design note for two unfinished milestones:
[share fixed capacity by demand](../PLAN.md#share-fixed-capacity-by-demand) and
[adapt batching and layout with feedback control](../PLAN.md#adapt-batching-and-layout-with-feedback-control).
PLAN.md owns the sequence; [architecture](architecture.md#memory-and-physical-size)
owns current behavior and limits. Every control named under "Proposed design" is
a proposal, not an available setting.

**User example:** configuring room for many tables must not give each empty
table private space for its maximum row count. If most requests touch `orders`,
that table should use the shared capacity. When a report starts reading `sales`,
capacity that is safe to reuse should follow it. Total reserved memory stays
the same.

## What exists today

All of these are fixed at startup; none is a workload controller.

| Shared resource | Settings |
|---|---|
| Ordinary-table resident row metadata, instance-wide; empty tables own none | `row_overlay_slots` (`large_object_pages` is separate) |
| Row bytes: two buffers for active data and relocation; committed MVCC history (none in locking mode) | `memtable_bytes`, `row_history_slots` |
| Foreground retained row IDs and DML buffers | `row_work_bytes`, `row_work_leases` |
| Reader table definitions | `definition_image_slots` |
| Procedural execution state | `execution_control_bytes`, `execution_control_leases` |
| Table columns, constraints, pending edit mappings, index bindings | `table_metadata_bytes` (starter 64 MiB), `table_metadata_leases` (8,192) |
| Analyzed column statistics; index counters | `statistics_bytes`, `statistics_leases`; `index_statistics_slots` |
| RAM and disk block caches, CLOCK eviction across objects | fixed slots; the disk file's logical length does not guarantee physical disk space |

Checkpoint and relocation buffers keep independent guaranteed-progress capacity
and never borrow foreground space. Byte pools round each extent to 64 bytes and
charge old and new extents while a buffer moves, so fragmentation can exhaust a
pool whose total free bytes look sufficient; the error names requested, free and
largest contiguous bytes and free descriptors.

On the I/O side, the checkpoint writer emits compressed row groups for small
deltas and column-oriented (PAX) row groups for full slices; column reads start
missing selected columns before yielding; read-ahead uses the configured request
slots minus one for the current read; physical output batches fill free upload
slots; and group commit shares publication within one event-loop turn without
a timer to collect more arrivals.

## Proposed design

### Keep capacity fixed; change who can use it

- Backing pools, their indexes and any controller state are reserved at startup.
  A **lease** is temporary ownership of part of that space; tables, queries and
  maintenance acquire and release leases without growing the heap.
- Keep cheap table identities separate from expensive resident row capacity.
- Classify capacity before sharing it. A verified cache block is evictable;
  uncommitted changes, a reader's selected version and an unfinished upload are
  not. Reassign only released capacity, and keep ownership stable through
  rollback, savepoints, table replacement and cold recovery.
- Reserve forward progress for commit, rollback, demand reads and required
  maintenance. When all suitable capacity is busy, use bounded reclamation and
  explicit backpressure, report pinned capacity, and fail with a named
  exhaustion error.
- Keep memory, cache-disk and journal/temporary limits separate. If cache files
  need guaranteed physical disk, implement and verify it; file length is not a
  reservation.
- Control changes reusable capacity and admission only, never SQL visibility,
  types, errors, isolation, durability or once-only effects.

### Follow the workload

| User workload | Intended response within the fixed budgets |
|---|---|
| Repeated single-order lookups | Retain useful row/index blocks; issue demand reads promptly |
| A report selecting two columns of a wide table | Fetch needed column groups; keep a large scan from evicting frequently reused blocks |
| Many small order updates | Give active write buffers capacity; drain output without starving reads |
| Sustained bulk ingestion | Group compatible queued output and overlap uploads; preserve commit order |
| Traffic moves to another table | Release unused leases and move reusable capacity after bounded observation |
| A long report keeps old versions | Keep those versions owned and expose the pressure instead of reclaiming them |

Choose whole-container versus ranged reads from selected bytes, reusable
contents, request overhead and observed transfer cost. Decide future row/column
layout far more slowly than request scheduling, accounting for rewrite cost and
retained readers, so a short burst never triggers a rewrite.

### Batch for throughput, dispatch promptly when idle

An isolated demand read or commit starts immediately. Under sustained arrivals,
already queued compatible work is grouped until a byte bound or the oldest
request's collection deadline. Speculative read-ahead and duplicate (hedged)
requests yield before demand reads, commit publication or required maintenance
can starve.

### Feedback control

Proposed explicit inputs, all bounded by startup reservations:

| Input | Purpose |
|---|---|
| Policy | `fixed` or a named adaptive controller |
| Hard ceilings | Upper bound of each active window or lease share; never above owned capacity |
| Minimum progress | Floor reserved for demand reads, commit publication and maintenance |
| Sample interval and minimum sample count | When a decision may be made, and on how much evidence |
| Maximum batch delay | Longest time the oldest request waits to be grouped |
| Maximum batch bytes | Largest grouped request or upload |

Expose the current active limits and why each changed. A latency target guides
policy; it does not guarantee completion during an object-store outage.

Candidate controllers, compared by measurement rather than chosen by
sophistication:

- **Simple bounded feedback:** step the limit up while latency stays within
  target, back off on overshoot (in the style of the references below).
- **PI (proportional-integral):** correct in proportion to the current error
  plus its accumulated history, removing steady offset.
- **PID (adds a derivative term):** reacts to the error's rate of change, but
  amplifies noise; delayed completions, mixed object sizes and discrete leases
  make it the hardest to tune.

Rules for any controller:

- Clamp every output to actual capacity and limit the change per interval.
- Filter noisy samples and require the minimum sample count before acting.
- Avoid oscillation with hysteresis (a dead band where no change happens) and a
  minimum time between changes.
- Anti-windup: when capacity is owned and cannot be reclaimed (a long reader, an
  unfinished upload), stop accumulating integral error rather than building a
  demand for capacity that does not exist.
- Do not let idle periods or failed requests count as successful low-latency
  samples.

## Evidence required before calling this implemented

- **Signals:** queue time separate from object service time, useful versus
  transferred bytes, active and waiting leases, pinned bytes, cache reuse and
  eviction, wasted read-ahead, batch fill, maintenance backlog and each
  controller decision. Sampling buffers and latency distributions must also fit
  startup budgets.
- **Workloads:** idle, burst, sustained and cooling traffic; switching hot
  tables; point reads mixed with scans; narrow-column reports; small updates and
  bulk writes; bandwidth changes, delayed and failed responses, cancellation,
  long-lived readers and maintenance pressure.
- **Proofs:** fixed memory and file bounds, capacity conservation, no runtime
  allocation, safe release after errors, fair progress, and unchanged recovery
  and transaction results. Keep the existing
  [ownership/exhaustion tests](https://github.com/e6qu/pos3ql/blob/b3aa3099993c351e09a0eb73fd6c1de67228007d/src/sql/tests/shared_row_work.rs)
  and [two-table spill/recovery case](https://github.com/e6qu/pos3ql/blob/b3aa3099993c351e09a0eb73fd6c1de67228007d/tests/external/run.sh).
- **Comparison:** fixed versus adaptive policies at equal resources on MinIO and
  SeaweedFS with both vanilla PostgreSQL controls from
  [performance](performance.md). Publish latency distributions, throughput,
  requests, bytes, resource use, useful prefetch, fairness, regressions and
  settling time after a load change, then pick the policy and numeric targets
  from that evidence. Memory comparisons use the
  [startup capacity matrix](performance.md#measuring-shared-startup-capacity)
  at identical explicit capacities.

## Design references

- [Envoy adaptive concurrency](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/adaptive_concurrency_filter.html)
  adjusts outstanding request limits from sampled latency.
- [Netflix concurrency-limits](https://github.com/Netflix/concurrency-limits)
  describes Vegas and Gradient2 feedback and partitioned concurrency guarantees.
- [Åström and Murray, PID control](https://www.cds.caltech.edu/~murray/books/AM05/pdf/fbs-pid_01Jan19.pdf)
  explains filtering, saturation and anti-windup.
