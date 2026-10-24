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

SQL execution still uses one CPU worker. Object-store I/O overlaps and batches
requests; commits retain durable order. Login-trigger commits yield while
waiting for durability and run their triggers once. State-changing statements use a
separate synchronous read client where replay could repeat effects. Metadata
capture and some maintenance reads also remain synchronous. Same-host MinIO
and SeaweedFS checks establish portability; they do not prove production speed.
See the [current contract and examples](docs/implemented-baseline.md).

## Remaining work

### Finish shared publication and bounded maintenance

**Example:** replacing a table must not invalidate a report that already selected
its rows or definitions. Cleanup should let ordinary queries run between turns.

Extend statement admission to consistent table/type definitions, rows, index
bindings and comments, including lookups after a query has begun. A report must
not combine its selected order rows with a replacement table's columns or
later comments. Connect the guarded table/type publication and retirement paths
to retained catalog versions before several CPU workers run statements.
Extend the table-shape checks used by index keys to every later catalog lookup.
Preserve PostgreSQL's distinction between listing catalog rows and resolving an
object for use. For example, after another connection renames `orders` to
`sales`, a repeatable-read query of `pg_class` still lists `orders`, while
`to_regclass('sales')` resolves the current name. Functions use current definitions
and sequence counters keep advancing. Do not freeze every metadata lookup at
the row snapshot. Compare both paths with PostgreSQL 18, including replacement
objects, comments, enum labels, private changes and rollback.
Keep rewrite history cleanup paced during concurrent publication.
Keep old row, index and backup data until their readers have finished.
Check reader and backup retention while old files are replaced. Recovery may
retain exclusive startup access; ordinary publication must let unrelated
operations progress under their own owners.
Some metadata capture and maintenance reads still block the worker. Raising
memory limits is not a completion criterion. The
[current behavior guide](docs/implemented-baseline.md) owns the implemented
table, routine, type, account and saved-query guarantees.

Done when publication and recovery preserve complete transactions, identity reuse cannot expose
replacement data to old readers, deletion respects readers and backups, and
rollback and exhaustion release every owner within startup reservations.
Lost commit replies must resolve to all of the transaction's changes or none.
Waiting and resuming a command must preserve its effects exactly once, including
event triggers and sequence values.

### Run statements on startup-reserved CPU workers

**Example:** several connections can use several CPU cores while preserving each
connection's command order.

Protect remaining shared execution state, replace the serial dispatcher with
fixed workers, and preserve database identity, fair queues, cancellation, storage
waits and commit order. A global lock that runs every statement in turn does
not satisfy this goal. Measure useful scaling for read, write and mixed loads,
including overload. Storage I/O already overlaps; execution on several CPU cores is separate.

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
