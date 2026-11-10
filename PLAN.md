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

### Retain query progress and finish application compatibility

**Example:** inserting an order writes an audit row and issues an order number.
A trigger then waits for storage or another connection's lock. Resuming must
continue that order with the same local values, audit row and number. Another
client must make progress while the storage response waits.

Object requests already overlap. The remaining execution work makes more SQL
paths safe to pause; increasing request slots alone cannot do that. Ordered
commit publication does not require serializing every query or independent
upload. Keep synchronous protection on each path until its unfinished work has
an owner that survives returning to the scheduler.

The implementation retains ordered AFTER work, nested INSERT/UPDATE/DELETE
completion, trigger frames and completed calls within unfinished `PERFORM` and
`SELECT ... INTO`, including unfinished PL/pgSQL scalar and table-function
bodies inside those owned expressions, declaration initializers and their type
checks. SQL-language functions retain their body statement, unfinished child
calls and completed nested writes, including rows saved before AFTER triggers
finish. SQL scalar return checks and per-row table checks share the same saved
call context. The [request path](docs/architecture.md#the-request-path)
owns their contracts and regression evidence. General function and query
execution still needs the remaining ownership work below.

Complete these connected execution paths:

- Extend function resumption to callers and expression positions without an
  owned query. Preserve completed PL/pgSQL bodies before return conversion that
  can wait, and retain write-row phases before a callback can yield. Scalar and
  table functions need the same guarantees, including one
  timing count per completed call; output width is independent of the
  input-argument limit.
- Finish write phases before and around AFTER work: BEFORE/INSTEAD triggers,
  source scans, row locks, partitions, constraints and transition collection.
  Include combined commands, rules, data-modifying common table expressions,
  COPY, replication apply and deferred triggers. Retain the original command
  and rollback boundary, rather than undoing inserted rows before a constraint
  can inspect them. Replace the explicit row-progress rejection for multirow
  updates with an owner for completed rows and the current callback; prove that
  a child reads earlier changed rows before the next row is written. Each insert/update/delete operation keeps its own completed
  transition table and firing order.
- Preserve remaining expression values, complex query positions and cursor
  progress. After the first result flush, continue with one header, every row
  once and one completion. `FETCH` must not execute effects for unfetched rows;
  saving a held cursor at commit must also yield during storage waits. Keep
  declared record and domain types after its source query ends. The
  [visibility milestone](#finish-transaction-visibility-and-keep-maintenance-moving)
  owns the overlapping reader and cleanup requirements.

Acceptance applies across every path:

- Keep the originally selected definitions, checked inputs, values, settings,
  effective user, command visibility, timestamps and deadline. Completed
  sequence calls, advisory-lock holds, input checks, cursor operations, audit
  writes and already delivered rows happen once, including repeated waits and
  a new call after resumption.
- Failed capture leaves the live call usable; failed attachment preserves the
  saved owner and caller. Recycling temporary memory cannot change saved text,
  arrays, records or NULL field types. Private waits bypass user error handlers.
  Preserve local and remote savepoint boundaries and originally selected row
  and type definitions without extra full-size copies. Existing metadata,
  warnings, checked-input and expression-binding regressions remain mandatory;
  their contracts belong in the [architecture](docs/architecture.md) and
  [compatibility guide](docs/postgresql-18-compatibility.md).
- Cancellation, timeout, rollback, connection closure and successful completion
  release owners. A handled timeout stays consumed if its handler waits.
  Memory remains inside the existing startup pools, with named exhaustion and
  no heap growth. Preserve 4,096-row results and repeated loops within the
  original 4 MiB statement arena. Repeated calls and catches reuse capacity.
- Demonstrate another client's progress with an actually delayed object-store
  GET, including empty-cache reads, both transaction modes, simple and extended
  protocols, triggers and nested functions. Compare the same SQL fixture with
  PostgreSQL 18: rows, audit writes, sequence values, types, errors and rollback.
  Keep allocation-forbidden, memory-ownership and failed-restoration coverage.

Audit PostgreSQL 18 built-ins by signature and executable behavior; catalog
presence alone is not support. Keep real SQLAlchemy, Alembic, psycopg2 and other
client comparisons. Complete all four full workflows on the finished revision,
including Linux amd64/arm64 and macOS arm64. Cancelled builds and skipped tests
are incomplete qualification. The native macOS mutex gate runs all ownership
cases and a direct Apple API control with ThreadSanitizer. Qualify the final
head using the permanent workflows.

Preserve explicit coverage/native profiles, assertions and all native targets.
Measure build and execution separately within unchanged job limits; bound
compiler concurrency and debug data instead of increasing timeouts. Each runner
job has its own bounded timeout, independent of shorter step timeouts. The
[contribution guide](CONTRIBUTING.md) owns the verification procedure.

### Share fixed memory and disk capacity according to demand

**Example:** configuring many possible tables must not reserve a large private
row lookup structure for each empty table. If most traffic touches `orders`, it should use
available shared slots. A long report keeps its selected versions until it
finishes; those slots cannot be reassigned simply because demand changes.

Ordinary row metadata, foreground row work, actual table definitions, index
bindings and analyzed columns share explicit fixed pools. Index counters have
an independent global limit, and SQL generators use the shared sequence catalog.
Readers keep their selected owners; a change reserves replacements before
durability. [Architecture](docs/architecture.md#memory-and-physical-size)
owns the current limits and admission behavior.

Retain the ownership, exhaustion, prepared recovery and once-only SQL tests as
shared resources change. Reserve replacements before effects and preserve old
images on rollback. For example, if analyzing `orders` cannot reserve statistics
space, its previous statistics must remain usable. A waiting commit must not
run an audit trigger again. Joined and recursive row decoding must continue to
include its complete physical shape, including hidden fields.

Reduce other catalog reservations using measured component costs. Empty tables
still have a cost; shared schema storage does not establish an embedded memory
minimum. Establish whether cache files need guaranteed physical disk reservation,
then implement and verify it rather than equating file length with reserved disk
space.

Keep the [ownership/exhaustion tests](https://github.com/e6qu/pos3ql/blob/b3aa3099993c351e09a0eb73fd6c1de67228007d/src/sql/tests/shared_row_work.rs) and
[two-table spill/recovery case](https://github.com/e6qu/pos3ql/blob/b3aa3099993c351e09a0eb73fd6c1de67228007d/tests/external/run.sh) as regression coverage.
Saved generations can exceed resident capacity; replaying their newer journal
tail still needs resident slots and bytes. Extend the
[measurement matrix](https://github.com/e6qu/pos3ql/blob/main/tools/measure-embedded-capacity.py) with realistic populated
sessions and pinned-reader pressure before claiming an embedded minimum.

Reserve forward progress for commit, rollback, required reads and maintenance.
Use bounded reclamation and report pinned capacity and named exhaustion.
Control applies to reusable capacity and admission, never SQL visibility or
durability. Keep memory, cache-disk and temporary/journal limits distinct.
The [resource design](docs/adaptive-resources.md) records candidate policies and
code evidence; proposed controls are not current settings.

Keep the 1/8/32/128-table comparisons at equal explicit global capacity in both
transaction modes, including narrow/wide schemas, retained readers and the
temporary overlap during schema/index replacement. Identify memory counters
and their units; separate named
startup reservations, resident pages, stack and TLS costs. Old unlabelled RSS
peaks are not comparable to the corrected current-program measurements described
in the [performance guide](docs/performance.md). Keep controller state within
startup budgets and prove capacity conservation, no runtime heap growth and safe
release after errors.
Preserve the [historical embedded measurements](https://github.com/e6qu/pos3ql/blob/4054a6d3c79e916728f8e84d84139eae6d86b501/benchmarks/baselines/2026-10-30-embedded-lazy-definitions/README.md);
compare future measurements at identical explicit capacities.

### Adapt batching, admission and future layout to the workload

**Example:** an isolated lookup starts immediately. During sustained traffic,
compatible queued reads and writes share bounded batches for throughput. A
nightly column report must not evict every frequently reused lookup block.

Retain fixed object-request slots, backing arenas and disk limits. Adjust active
windows, lease ownership and compatible batch sizes inside them. Bound queue
delay and bytes; protect demand reads, commit publication and required
maintenance before speculative read-ahead or duplicate requests. Use measured
column demand, reuse, update rate and rewrite cost when choosing future immutable
row/column output and ranged versus whole-object reads. Avoid rapid layout
rewrites after short bursts.

Compare simple feedback with proportional-integral (PI) and
proportional-integral-derivative (PID) control using explicit policy, ceilings,
minimum progress, sample interval/count and maximum batch delay/bytes. Clamp
changes, filter noisy samples, avoid repeated reversals, and prevent integral
error accumulating when owned capacity cannot be reclaimed. Choose the policy
from measurements rather than assuming a more elaborate controller is better.

Compare fixed and adaptive policies at equal resources on MinIO and SeaweedFS,
with both vanilla PostgreSQL controls. Test idle/burst/sustained/cooling traffic,
point/scan mixtures, narrow columns, bulk writes, delay/failure, cancellation,
long readers and maintenance pressure. Retain latency distributions, throughput,
requests/bytes, useful prefetch, cache/pinned usage, fairness and controller
settling. Correctness, durable ordering and startup limits must hold throughout.

### Finish transaction visibility and keep maintenance moving

**Example:** a report must finish using the rows and definitions it selected,
even when another connection replaces a table or maintenance replaces saved files.
A waiting schema edit must never attach itself to a new object that reused a name.

- When a report lists an object's saved name, its later column, index, comment
  and dependency reads must still refer to that same object. Unfinished edits
  must stay private to their transaction. Preserve repeated dependency rows.
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
  Extend retained execution to function calls and effects, joins, ordering and
  row locks. Make commit-time result saving yield during storage
  waits, then reuse the positions for ordinary results after their first flush.
  Keep read counts through savepoint rollback and release; never transfer a
  retired table’s pending counters to a replacement that reuses its slot.
  Preserve the existing [cursor input, type and capacity rules](docs/postgresql-18-compatibility.md#capacity-boundaries),
  declared result types, captured inputs and settings, bound operators, reader
  ownership, failed-fetch cleanup and scan statistics. Fetching, skipping and
  moving rows must keep their distinct PostgreSQL effects. Sequence values
  already issued remain owned by the command across retries and rollback.
  Finishing or saving a result must release its input owners across embedded
  calls and wire command paths, including COPY and extended-protocol Sync.

Follow PostgreSQL's distinction between listing an old catalog row and resolving
an object for current use. After `orders` becomes `sales`, an older report can
still list `orders` while `to_regclass('sales')` resolves the current name.
Freezing all lookups at the report's position would be incorrect.

Acceptance: while reads, schema edits, publication and maintenance overlap,
readers keep their selected objects and values, and new reads see whole commits.
Reusing a name or slot never redirects a reader to replacement data. Cleanup
respects readers and backups. Preserve the existing rollback, savepoint,
prepared-transaction, capacity and empty-cache recovery checks throughout this
conversion. Reject corrupt or stale restored state before accepting SQL writes.
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
Run shared formatting, Rust type checks and lint before admitting build and test matrices.
Keep native SQL fixtures in native gates and interpreter ownership cases beside
their runtime modules, so interpreter startup does not compile irrelevant fixtures.
Shared fixture helpers remain available to ownership cases; native storage tests
do not enter interpreter startup. Keep time-only timezone expectations independent
of the day or season on which CI runs.
Reclaim each polling snapshot's scratch space instead of relying on a favorable
thread schedule to finish before the arena fills.
Keep compiler and per-test progress visible through CI cancellation while
diagnosing deadline overruns; preserve the existing job budgets.
Require both ownership groups to complete successfully and cover the full named
manifest without missing, repeated or ignored proofs.

No runtime heap growth, growing pools or allocating sorts. Keep consequential
settings explicit and avoid needless wrappers and fallbacks. Capacity increases
are not a completion criterion. Keep current contracts in their
[owning guides](docs/README.md), open acceptance criteria here, and historical
observations with their original [evidence](benchmarks/README.md). The roadmap is
finished only when every remaining acceptance criterion is demonstrated.
