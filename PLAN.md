# Finish pos3ql

[Overview](README.md) · [What works today](docs/implemented-baseline.md) ·
[Compatibility limits](docs/postgresql-18-compatibility.md) · [Contributing](CONTRIBUTING.md)

## The destination

Use PostgreSQL SQL and tools with object storage as the durable copy. A confirmed
order must survive loss of the server's disk. A slow download or upload must
allow unrelated work to progress. Memory limits are chosen before startup.

One server owns the right to publish changes to a prefix. That does not require
performing every storage request one at a time. SQL execution on several CPU
workers is separate from overlapping network requests.

Current SQL execution is serial. There are already multiple download slots,
read lookahead, immutable journal batches, packed row groups, and independently
readable column groups. These pieces do not yet provide the complete concurrent
read and write path below. Same-host MinIO and SeaweedFS tests establish
portability; they do not establish production performance.

## Current PR: finish object-store reads and writes together

Keep implementation, regressions, recovery, capacity accounting and documentation
in one PR. Do not merge a partial implementation.

The current branch connects concurrent upload clients, ordered durable commit
completion, selected-column read batches and explicit transaction modes. This
implementation is still being validated. Shared read ownership and cancellation
now retain results for each waiting query. Physical output batching and complete
failure/recovery evidence remain acceptance work in this same PR.

### Concurrent and batched reads

**Example:** two customers request different uncached invoices. Both downloads
can start before either finishes. A cached invoice remains available meanwhile.
A report asking for two columns should download those columns together, without
fetching the large notes column it did not request.

- Start multiple independent requests within explicitly configured startup slots.
- Share requests for the same data; preserve completed bytes and failures until
  their consumers can handle them. Cancellation must release abandoned work.
- Issue a bounded batch of selected column or scan reads before waiting for the
  first response. Preserve validation, selected versions and completed work on
  resumption; never repeat a data-changing expression.
- Release shared cache and storage owners before waiting on network I/O.

### Concurrent and batched writes

**Example:** two customers save different orders. Their changes can share one
durable log upload. Independent immutable uploads can overlap, but neither
customer receives success before their change is recoverable.

- Reserve upload clients, immutable request bytes, progress and completion queues
  at startup. Slots and batch sizes are explicit settings.
- Allow independent uploads to complete out of order; publish references and
  commit outcomes in the required order. A root must never name incomplete data.
- Batch committed journal records and physical output. While storage waits,
  continue unrelated queries, cancellation and bounded maintenance.
- Reject work after an ownership probe detects a replaced writer, before a new
  commit enters the journal. Flush an uncertain commit's terminal error before
  closing its connection.
- Preserve writer fencing, retries, savepoints, prepared transactions and cold
  recovery. Never turn an uncertain commit into a claim that it rolled back.
- Keep reads, durable commits and maintenance from starving one another.

### Physical layout

**Example:** a report sums invoice amounts without downloading descriptions;
updating one order appends a small change instead of rewriting the full table.

- Use immutable packed objects and column groups, with small logged row changes.
  Reuse existing groups when their data has not changed.
- Include block type in immutable content identities, so equal payloads in
  different formats cannot overwrite one another. Verify rejection of old headers.
- Keep column boundaries and offsets explicit and checked. Choose alignment for
  decoding and useful byte-range requests, not an assumed object-store sector.
- Measure requests, downloaded bytes and rewritten bytes. Demonstrate batching
  and projection savings, including with both caches empty.

### Explicit transaction mode

**Example:** with MVCC enabled, a report can keep reading the earlier order
while another transaction updates it. With MVCC disabled, conflicting work
waits on locks.

- Require an explicit mode. MVCC means retaining older committed values for
  readers; locking mode keeps current committed values and waits on conflicts.
- Preserve supported PostgreSQL isolation behavior in each mode. Reject an
  isolation request that cannot be implemented correctly; never silently weaken it.
- Cover lock upgrades, deadlocks, cancellation, savepoints, triggers, COPY,
  replication, transaction end, bounded history, and recovery.
- Count all retained state at startup and name exhaustion errors.

### Completion evidence

Exercise actual overlapping HTTP GETs and PUTs, batched requests, duplicate
requests, out-of-order completion, transport failures, cancellation, capacity
pressure and fairness. Verify durable acknowledgement and cold recovery against
MinIO and SeaweedFS, and SQL results against vanilla PostgreSQL 18. Keep complete
CI passing on the final PR head without increasing timeouts. These checks
establish correctness and request costs; speed claims require the measurements
below.

Design references: Neon overlaps reads and [parallel S3 uploads](https://neon.com/blog/recent-storage-performance-improvements-at-neon),
but [acknowledges a replicated log before S3](https://neon.com/docs/introduction/architecture-overview).
[SlateDB](https://slatedb.io/docs/design/writes/) batches log writes to object
storage and tracks durability separately from acceptance. These are design
references, not pos3ql performance evidence or permission to acknowledge
local-only data.

## Remaining work after storage I/O

### Finish shared publication and bounded maintenance

**Example:** replacing a table must not invalidate a report that already selected
its rows or definitions. Cleanup should let ordinary queries run between turns.

Finish protection for engine-wide publication, type-name changes, table rewrites,
checkpoint publication and object deletion. Retain historical row, index and
backup references before publishing replacements. Finish bounded heap remapping,
post-checkpoint eviction and critical-pressure work; capture and reader-history
cleanup already use bounded turns.

Done when failed publication leaves valid state, identity reuse cannot expose
replacement data to old readers, deletion respects readers and backups, and
rollback and exhaustion release every owner within startup reservations.

### Run statements on startup-reserved CPU workers

**Example:** several connections can use several CPU cores while preserving each
connection's command order.

Protect remaining shared execution state, replace the serial dispatcher with
fixed workers, and preserve database identity, fair queues, cancellation, storage
waits and commit order. A global lock that runs every statement in turn does
not satisfy this goal. Measure useful scaling for read, write and mixed loads,
including overload. This is separate from the current PR's object-store I/O.

### Publish realistic performance and recovery evidence

**Example:** a user can compare typical and slow request times, required hardware,
storage traffic and what happens when a server fails.

Run the [measurement suite](docs/performance.md) repeatedly on fixed hardware
with independently operated object storage. Discover and record hosts, network,
routing and owner-only configuration paths without publishing credentials.

Keep both vanilla PostgreSQL 18 controls: host-available resources and the same
CPU/memory allowance as pos3ql. PostgreSQL uses ordinary durable storage; report
its medium and settings alongside pos3ql's object store and caches.

Measure warm and empty caches, writes, joins, maintenance, large catalogs,
workers, logical replicas, delay, recovery, and failover across machines through
real client routing. Retain raw results, revisions, latency distributions,
throughput, CPU, memory, access paths, object requests and transferred bytes.
No representative deployment run has been published yet. Shared-host and hosted
runner timings remain exploratory.

## Rules throughout

Match supported PostgreSQL SQL, SQLSTATEs, types, catalogs and wire behavior.
Reject unsupported behavior clearly. Earlier pos3ql versions are not compatibility
targets. Keep [durable formats](docs/durable-format.md) and current recovery
checks accurate whenever bytes change.

No runtime heap growth, allocating sorts or growing pools. Use explicit settings
and consequential arguments, no silent fallbacks or needless wrappers. Fix every
encountered defect in this PR unless an actual external blocker prevents it.
Diagnose timeout causes; never increase durations to conceal them.

Keep current contracts in their [owning guides](docs/README.md), unfinished
work here, and historical observations with their [evidence](benchmarks/README.md).
The roadmap is complete only when all remaining acceptance criteria are met.
