# Finish pos3ql

[Overview](README.md) · [Architecture](docs/architecture.md) ·
[Current behavior](docs/implemented-baseline.md) ·
[Limits](docs/postgresql-18-compatibility.md) · [Contributing](CONTRIBUTING.md)

## The destination

Applications use supported PostgreSQL SQL and clients while object storage holds
the durable copy. A confirmed order survives loss of the server's disk. Reports,
updates and maintenance make progress without unrelated work waiting on every
storage request. Memory and concurrency are chosen before startup; exhaustion
returns an explicit error.

One active process publishes to a prefix containing the instance's SQL databases.
Its commit history must retain durable order. Reads, independent uploads and SQL
CPU execution do not all need to be serialized to preserve that order.

Today, object-store I/O overlaps and batches requests, while SQL CPU work uses
one worker. Some state-changing, metadata and maintenance reads still wait
synchronously. Same-host MinIO and SeaweedFS checks establish portability;
they do not establish production performance. The owning guides describe
[current behavior](docs/implemented-baseline.md), [physical limits](docs/architecture.md#memory-and-physical-size)
and [what has actually been measured](docs/performance.md).

## Remaining work

### Complete coherent publication and bounded maintenance

**Example:** a report must finish using the rows and definitions it selected,
even when another connection replaces a table or maintenance replaces saved files.
A waiting schema edit must never attach itself to a new object that reused a name.

- Connect retained catalog rows to every later definition, column, index,
  comment and dependency lookup. Preserve exact duplicate dependency counts.
  Finish source-lifetime checks and private edits for remaining catalog families.
- Finish lifetime SQL identifiers beyond the families guaranteed in the
  [behavior guide](docs/implemented-baseline.md#schema-changes-and-object-identity).
  Check shared catalogs and database-template copies, including recovery and
  comments that must stay attached when physical catalog slots change.
- Complete reader-safe row/index history and backup retention. Pace pruning,
  replacement and deletion without holding unrelated owners.
- Make remaining synchronous metadata and maintenance storage reads yield with
  bounded retained state. Recovery can keep exclusive startup access; ordinary
  operation must allow unrelated work to progress.

Follow PostgreSQL's distinction between listing an old catalog row and resolving
an object for current use. After `orders` becomes `sales`, an older report can
still list `orders` while `to_regclass('sales')` resolves the current name.
Freezing all lookups at the report's position would be incorrect.

Acceptance: complete transactions publish and recover together; source reuse
cannot redirect retained data; cleanup respects readers and backups. Cover
rollback, savepoints, prepared transactions, fixed-capacity exhaustion and
recovery after deleting local caches. Reject corrupt or stale restored state
before writer promotion. Resumption preserves effects once, including triggers
and issued sequence values. Lost commit replies resolve to all changes or none.

### Execute SQL on startup-reserved CPU workers

**Example:** independent connections can use several CPU cores while each
connection preserves command order and every confirmed transaction stays recoverable.

Finish shared execution-state ownership, then replace the serial dispatcher with
fixed workers. Preserve database/account identity, fair queues, cancellation,
locks, storage waits and commit order. A global lock running all statements in
turn does not meet this goal. Measure read, write and mixed-load scaling and
overload; extra connection or object-request slots alone do not establish it.

### Qualify realistic size, performance and recovery

**Example:** a user can choose hardware from measured typical and slow response
times, sustainable transaction rates, storage traffic and failure behavior.

Discover and record fixed hosts, independent object storage, network/routing and
owner-only configuration paths without publishing credentials. Run the
[measurement suite](docs/performance.md) repeatedly. Keep both vanilla PostgreSQL
18 controls: host-available resources and the same CPU/memory allowance as pos3ql.
Record PostgreSQL's ordinary durable medium and pos3ql's object store/cache tiers.

Cover useful data sizes, warm/empty caches, reads/writes, joins, large catalogs,
maintenance, workers, logical replicas, delays, recovery and multi-host failover
through actual client routing. Retain revisions, raw results, errors, latency
distributions, throughput units, CPU/RSS, object requests, bytes and cost inputs.
No representative deployment run or qualified production size/TPS/bandwidth
claim has been published. Theoretical addressability and calculated examples
are not acceptance evidence.

## Rules throughout

Fix encountered defects in the same complete PR unless an actual blocker
prevents repair. Diagnose failures and timeouts; do not extend limits to hide
them. Match supported PostgreSQL SQL, types, errors, catalogs and wire behavior,
including one correct command-completion response per statement and transactional
cleanup of temporary objects.
Reject unsupported behavior and stored formats explicitly. Earlier pos3ql
versions are not compatibility targets.

No runtime heap growth, growing pools or allocating sorts. Keep consequential
settings explicit and avoid needless wrappers and fallbacks. Capacity increases
are not a completion criterion. Keep current contracts in their
[owning guides](docs/README.md), open acceptance criteria here, and historical
observations with their original [evidence](benchmarks/README.md). The roadmap is
finished only when every remaining acceptance criterion is demonstrated.
