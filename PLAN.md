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

**Example:** a query reads three orders, increments a session counter in its
first result column and writes an audit row through a function in its second.
It must produce three increments, three audit rows and three result rows,
including when a function pauses. Completed columns must not run again.

Supported single-table scalar statements retain their source, predicate and
projection positions through mutable function handoffs, using the existing
physical reader and startup arenas. Temporary call state belongs to active rows.
Calls retain selected identities, original defaults and null handling; explicit
variadic arrays remain distinct from expanded arguments.
Declared casts retain their selected conversions and evaluation order through
arrays and assignments, including typed NULL values. Array children convert in
order; `INSERT ... SELECT` converts each complete selected row before the next row.
Planning and dependency visits release their temporary schema descriptions,
including queries without stored-function calls. Repeated visits reuse the same
statement space; the small-arena enum and composite-query regression keeps its
original 256 KiB limit.
Nested query descriptions share that scratch safely. Ordinary casts do not
reserve checked-input request storage when they have no preparation-time checks.
Completed calls release body workspaces while preserving typed results and
command effects, including nested calls and table-function results.
Finish retaining values checked while SQL is prepared. For example, preparing
`SELECT '{approved}'::checked_status[]` can run a user-defined constraint;
describing or executing that prepared statement must reuse its checked value.
Cover implicit column and function arguments, repeated lock waits, cancellation,
stored definitions, renames and recovery. A SQL function checks its string body
at creation and again on its first runtime call in a backend, retaining inputs
for that routine definition and resolved call types. Accepted inlining instead
keeps the planner's checked values with its caller; rejected attempts leave the
runtime check separate. A cursor must preserve its own plan across opening
waits, separate fetches and holdable commit, releasing it on close or rollback.
A PL/pgSQL statement keeps its own plan across calls; dynamic `EXECUTE` makes
a new plan each time.
Unreached branches must not run constraints. Verify these differences against
PostgreSQL, including function settings and `check_function_bodies=off`.
Extended-protocol Bind must validate parameters before planning function inputs,
retain decoded values through planning waits, and publish its portal only after
preparation succeeds. Closing or cancelling one portal must release its plan
without changing another portal's checked values.
Keep parameterless generic plans shared and live while portals use them;
parameterized planning must use the supplied values when pruning branches.
`DISCARD PLANS` must change future preparation without repeating checks in an
already bound portal or cursor. Verify cache changes across
schema edits, commit, rollback, search paths and roles against PostgreSQL;
unrelated table changes must not repeat a constraint.
Compare procedural specialization, nested arguments, sized casts, selected
operator/cast support functions and inherited ownership with PostgreSQL.
Preserve the distinction between a constant argument and the element placeholder
used when converting an existing array; their input-check timing can differ.
Cover index, policy, rule and privilege changes, plus TRUNCATE commit, rollback
and prepared transactions, including immediate delivery to other sessions
before an ANALYZE commits. Retain the selected table identity through these
boundaries. Match whether whole-relation catalog statistics changed; repeating
unchanged statistics and renaming an index must not repeat a constraint.
Keep completed inputs while rolling back and safely replaying the unfinished
input within its original deadline. Caught input errors must retain their exact
attempt and error through a later wait, without repeating the constraint.
Cancellation must release unfinished
metadata so a fresh statement can select a different schema and track its
subsequent changes. Demonstrate the complete behavior in CI
before closing this acceptance item. The original PostgreSQL probe inputs, raw
results and verified provenance are retained under
`tests/external/reference/2026-11-03-input-plans/`.
Focused PostgreSQL 18.6 observations also cover cursor fetch timing, query-error
order, checked defaults, prepared values and canonical foreign-object labels.
Keep event reporting, object identification and address lookup consistent with
the same object kind; an unchanged owner still produces the expected DDL event.
Reassigning ownership must exclude the destination from the objects still to
change. Repeated same-owner commands must finish without consuming catalog
versions; foreign wrappers and servers must preserve ownership through rollback
and recovery.
Data-change trigger statements that need checked domain-array input currently
raise `0A000` before running the constraint; event triggers have an engine-owned
execution context. Finish a retained trigger continuation that preserves prior
row changes, local variables, branch position and transition rows while input
preparation waits. Verify that constraints see the preceding transaction and
statement changes, run once, and do not fire in unreached branches. Do not route
these checks through a retry that first undoes the rows they must observe.
Keep simple table, constant VALUES and one-row cursor expressions deferred until fetching,
including scalar calls, then extend those positions to the remaining cursor
plans that currently execute when opened. Ordinary PL statements without any
checked or planner inputs must not consume checked-plan slots. Keep running
images alive when `DISCARD PLANS` refreshes future query admissions.
Keep sequence effects aligned with PostgreSQL through sorting, skipped rows
and tied groups, including nested queries and disk-backed sorts. A zero-row
limit must validate the query without evaluating its input.
Procedural query handoffs must publish rows and local assignments at their proper
boundary. A real caught RETURN QUERY error preserves its completed row prefix;
a private retry must not publish that prefix twice. Compare static SELECT INTO's
one/two-row consumption with dynamic EXECUTE INTO, which evaluates the query fully.
Cold-recovery tests must explicitly enable object storage and synchronous journal
publication before deleting local files; starting an HTTP fixture alone proves
neither configuration nor durability.

Extend those positions through function bodies and object-store waits, complex
plans, named/custom casts, SQL cursors and results already sent to the client.
Preserve types, one result header, scan counts, cancellation, rollback and named
capacity errors. Compare effects and values with PostgreSQL, including empty-cache
reads, nested calls and replaced objects. The
[architecture](docs/architecture.md) describes the current boundaries.

Audit PostgreSQL 18 built-ins by signature and executable behavior, including
`hashtextextended('order-42', 0)`. Catalog presence alone is not support.
Keep values, declared types, nulls, errors and argument effects aligned with
PostgreSQL. Validate declared assignments before row processing: an empty input
or typed NULL must not bypass an otherwise invalid conversion. Run real
SQLAlchemy, Alembic and psycopg2 inspection, migrations,
streaming and binary-client workflows against both servers; preserve native
Linux amd64, Linux arm64 and macOS arm64 coverage.
Keep protocol preparation separate from an executed implicit command block.
Standalone prepared-transaction resolution must work through JDBC, while
unsynchronized preceding executions and explicit blocks still reject it. Query
analysis must pin snapshots at PostgreSQL's boundary; utility preparation must
permit transaction settings, and protocol errors must preserve correct rollback
and failed-block recovery. Aggregate support functions use the aggregate owner's
permission to call them while their bodies retain ordinary invoker identity.
Keep session plan ownership with its frontend while work moves into commit
workers or prepared slots. A suspended command must retain its original arena
and resume after a conflicting lock is released. Temporary argument tracking
must fit the original bounded-memory proofs without enlarging their arenas.
Match each standalone utility's publication boundary with PostgreSQL: creating a
database publishes before protocol `Sync`, while concurrent index commands have
intermediate committed catalog states and a final transaction. A common
"commit every utility immediately" rule is incorrect.

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
compare future measurements at identical explicit capacities. Add populated
multi-session and pinned-reader measurements before claiming an embedded minimum.

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

No runtime heap growth, growing pools or allocating sorts. Keep consequential
settings explicit and avoid needless wrappers and fallbacks. Capacity increases
are not a completion criterion. Keep current contracts in their
[owning guides](docs/README.md), open acceptance criteria here, and historical
observations with their original [evidence](benchmarks/README.md). The roadmap is
finished only when every remaining acceptance criterion is demonstrated.
