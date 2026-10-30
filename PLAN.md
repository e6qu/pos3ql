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
one worker. Journal cleanup, saved-file cleanup and row-file merges retain
their progress and yield while object-store responses wait. Checkpoint row
capture and index construction also retain their input and output positions.
Journal cleanup does not prevent checkpoints from freeing row-cache space.
State-changing commands and reads after the first result flush still have
synchronous paths.
Same-host MinIO and SeaweedFS checks establish portability;
they do not establish production performance. The owning guides describe
[current behavior](docs/implemented-baseline.md), [physical limits](docs/architecture.md#memory-and-physical-size)
and [what has actually been measured](docs/performance.md).

## Remaining work

### Close application compatibility and embedded memory gaps

**Examples:** an application can call `hashtextextended('order-42', 0)`, use
SQLAlchemy models and psycopg2 cursors, and upgrade or reverse its schema with
Alembic. An embedded database with many small tables should not reserve the
maximum row history independently for every possible table.

Audit PostgreSQL 18 built-ins by signature and executable behavior. Catalog
presence alone is not support. Compare values, declared result types, nulls,
errors and argument effects with PostgreSQL, including seeded hashes.
Registered routine identities, aggregate kinds and null-call properties must
match the live reference catalog even while missing routines are being added.
Run real SQLAlchemy, Alembic and psycopg2 workflows against both servers.
Relation names converted through `regclass` must remain resolvable outside the
search path, including quoted names and indexes. Catalog vectors must preserve
their element types and zero-based bounds through array functions used by
schema inspection. Quantified pattern matching must use pattern semantics.
Measure startup reservations and actual resident memory for empty and populated
embedded databases; report the per-table contribution separately. Remove
avoidable per-table row/history reservations through shared startup-owned
capacity, preserving stable reader identities, rollback and named exhaustion.
Reserve needed row history before the durable commit boundary; a smaller shared
pool must never fail for capacity after the commit is already durable.
Reject per-row history exhaustion before writing the commit too; a multirow
transaction must leave every row unchanged and keep the server usable on error.
Include the current execution-stack requirement in embedded measurements and
reduce avoidable wide construction frames alongside table and history metadata.

### Finish transaction visibility and keep maintenance moving

**Example:** a report must finish using the rows and definitions it selected,
even when another connection replaces a table or maintenance replaces saved files.
A waiting schema edit must never attach itself to a new object that reused a name.

- When a report lists an object's saved name, its later column, index, comment
  and dependency reads must still refer to that same object. Unfinished edits
  must stay private to their transaction. Preserve repeated dependency rows.
  Catalog sources must retain their real schema in planning and execution;
  `SELECT pg_catalog.pg_class.relname FROM pg_catalog.pg_class` must resolve.
  Preserve schema-qualified outer references and PostgreSQL's internal-character
  to text comparison used by application schema inspection.
- Keep older rows, indexes and saved files until readers and backups finish.
  Remove obsolete versions in limited batches so other queries keep moving.
  Files written by an unfinished checkpoint remain owned until its publication
  resolves; a reader finishing must not cause them to be collected as orphans.
  Switching output producers cannot cancel another job's immutable files;
  durable manifests never name output that has not finished uploading.
- Retain the query's scan and output position after sending a result prefix.
  Today a large result finishes its remaining reads synchronously once bytes
  leave the connection buffer, because restarting would send a second result
  header and repeat already-delivered rows. Resumption must preserve one header,
  every row once and one completion without occupying the worker during a wait.
  SQL cursors must use the same retained execution positions: `DECLARE` binds
  the query and its snapshot; `FETCH` advances it. For example, declaring a
  cursor over `nextval('order_numbers')` must not issue numbers for rows that
  have not been fetched. Row locks follow fetched rows. Holding a cursor across
  commit saves the required result before releasing its transaction.
  Simple cursors over one physical table retain their reader,
  built-in arithmetic/comparisons, `CASE WHEN` expressions, array construction,
  element access and slices, boolean filters, integer row limits/offsets and
  captured parameters. Preserve their declared common types through subqueries,
  null values and result inspection without issuing skipped effects. Preserve
  enclosing column types in nested table-function inputs, including empty
  and null arrays; describing a result must not require a sample row.
  Carry sequence effects through subqueries and row bounds without losing their
  connection state or repeating completed calls.
  Bind array-subquery elements from their declared types, including domains over
  arrays; selecting a domain value must not turn it into an extra array dimension.
  Forward-only cursors reuse memory per fetch; scrolling cursors retain visited
  results. Omitted scroll options follow the bound plan’s backward-read support.
  Operator choices and row-independent filters stay fixed after declaration.
  Normalize range predicates through the same comparisons and boolean rules:
  null bounds, repeated operands, custom operators and fetched-row errors must
  match PostgreSQL, including `BETWEEN SYMMETRIC`.
  Holdable versions save unread results during commit and release their input
  owners across simple queries, extended-protocol Sync, COPY and large-object calls.
  Extend retained execution to function calls and effects, joins, ordering and
  row locks. Make commit-time result saving yield during storage
  waits, then reuse the positions for ordinary results after their first flush.
  Keep read counts through savepoint rollback and release; never transfer a
  retired table’s pending counters to a replacement that reuses its slot.
  Preserve relation ownership, failed-fetch cleanup, scan statistics and
  PostgreSQL's distinction between fetching, skipping and moving rows.
  Preserve the existing [cursor input, type and capacity rules](docs/postgresql-18-compatibility.md#capacity-boundaries)
  while adding execution positions. Later variable assignments must not change
  captured inputs. Keep issued sequence values in the owned execution state so
  retries cannot issue them again. Preserve rollback, cleanup and PostgreSQL's errors for
  invalid fields and utility parameters. Saved dates must still use the
  settings chosen before `FETCH`, including after commit.

Follow PostgreSQL's distinction between listing an old catalog row and resolving
an object for current use. After `orders` becomes `sales`, an older report can
still list `orders` while `to_regclass('sales')` resolves the current name.
Freezing all lookups at the report's position would be incorrect.

Acceptance: while reads, schema edits, publication and maintenance overlap,
readers keep their selected objects and values, and new reads see whole commits.
Reusing a name or slot never redirects a reader to replacement data. Cleanup
respects readers and backups. Preserve the existing rollback, savepoint,
prepared-transaction, capacity and empty-cache recovery checks throughout this
conversion. Corrupt or stale restored state must fail before writer promotion.
Resumption preserves effects once, including triggers and issued sequence values;
skipped expression branches must not run their queries or effects.
Lost commit replies resolve to all changes or none.

### Run independent SQL commands on several CPU workers

**Example:** independent connections can use several CPU cores while each
connection preserves command order and every confirmed transaction stays recoverable.

Give each worker its own temporary execution state while protecting shared
rows, definitions, locks and transactions. Choose the fixed worker pool before
startup. Preserve database/account identity, fair queues, cancellation, storage
waits and commit order. A global lock running all statements in
turn does not meet this goal. Measure read, write and mixed-load scaling and
overload; extra connection or object-request slots alone do not establish it.

### Measure realistic workloads and server replacement

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
Keep native Linux amd64, Linux arm64 and macOS arm64 builds, lint and boundary
regressions passing; use the platform's declared C types at libc boundaries.

No runtime heap growth, growing pools or allocating sorts. Keep consequential
settings explicit and avoid needless wrappers and fallbacks. Capacity increases
are not a completion criterion. Keep current contracts in their
[owning guides](docs/README.md), open acceptance criteria here, and historical
observations with their original [evidence](benchmarks/README.md). The roadmap is
finished only when every remaining acceptance criterion is demonstrated.
