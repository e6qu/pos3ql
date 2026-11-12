# Finish pos3ql

[Overview](README.md) · [Architecture](docs/architecture.md) ·
[What works today](docs/implemented-baseline.md) ·
[Limits](docs/postgresql-18-compatibility.md) · [Contributing](CONTRIBUTING.md)

This plan lists only unfinished work. Current behavior belongs in the
[guides](docs/README.md); historical plans are preserved in the
[history index](docs/history/README.md).

## The destination

- Applications use supported PostgreSQL 18 SQL and clients, with explicit errors
  for anything unsupported.
- Object storage holds the durable copy. A confirmed commit survives loss of the
  server's disk.
- A slow storage request never stalls unrelated connections, and independent
  connections use several CPU cores.
- Memory, disk and concurrency limits are fixed before startup. Shared capacity
  follows demand, and running out gives a named error.
- A failed server is replaced from object storage through a tested procedure
  that reroutes clients.
- Users choose hardware from repeatable measurements against vanilla
  PostgreSQL 18.

One active process publishes to each object-store prefix and keeps commits in
durable order. That order does not require running every read, upload or SQL
statement one at a time.

## Where we are

SQL, transactions in `mvcc` and `locking` modes, PostgreSQL clients, indexes,
logical replication, backup and recovery from object storage work today and are
compared with PostgreSQL 18 in CI. Object-store requests overlap and commits
share uploads. SQL runs on one CPU worker. Many paths already pause during a
storage wait and resume later: reads, cleanup, merges, checkpoints, cursors,
PL/pgSQL and SQL-language functions, triggers, and the row selection of
`INSERT ... SELECT`, `UPDATE`, `DELETE` and `MERGE`. A write's row changes,
DDL, and any statement after its first effect still block that worker on a
storage read. No measurement on separate machines has been published.

## Milestones

Each milestone is a coherent piece of work that ships in complete PRs. The order
is the intended sequence. Compatibility work runs alongside the others.

| Milestone | Status | Depends on |
|---|---|---|
| [Pause and resume every query path](#pause-and-resume-every-query-path) | In progress | — |
| [Isolate readers and bound cleanup](#isolate-readers-and-bound-cleanup) | In progress | — |
| [Measure a first deployment baseline](#measure-a-first-deployment-baseline) | Not started | — |
| [Run SQL on several CPU workers](#run-sql-on-several-cpu-workers) | Not started | Pausing, reader isolation |
| [Share fixed capacity by demand](#share-fixed-capacity-by-demand) | Partly done | — |
| [Adapt batching and layout with feedback control](#adapt-batching-and-layout-with-feedback-control) | Designed | Baseline, capacity sharing |
| [Close application compatibility gaps](#close-application-compatibility-gaps) | Ongoing | — |
| [Qualify production deployment](#qualify-production-deployment) | Not started | All of the above |

### Pause and resume every query path

**Goal:** a statement waiting for object storage or another connection's lock
gives up the CPU worker and later resumes exactly where it stopped.

**Example:** inserting an order fires a trigger that writes an audit row and
takes an order number. The trigger waits for storage. Meanwhile other clients
run. On resumption the same order keeps the same local values, audit row and
number.

**Done today:** ordered AFTER-trigger work; nested INSERT/UPDATE/DELETE;
trigger frames; PL/pgSQL calls inside `PERFORM`, `SELECT ... INTO`,
declaration initializers and every PL/pgSQL statement's expressions,
including statements with several (`FOR` bounds, `CASE`, `RAISE` arguments
and options, `IF ... ELSIF`, `ASSERT`, `CALL` arguments, dynamic `EXECUTE`
values), which keep finished values across a wait; SQL-language function
bodies, child calls and return checks; domain `CHECK`s on a PL/pgSQL
function's result, which keep the finished result and the check's calls; PL/pgSQL procedures and `DO` blocks,
including nested `CALL`, resuming at the waiting statement after their own
`COMMIT` or `ROLLBACK`, with queries yielding on storage reads; SQL cursors executing from retained query state, including row locks
taken per `FETCH` and scrollable cursors that evaluate revisited rows again;
client output that never blocks the worker on a slow reader; large
single-table results resuming row by row across client, storage and lock
waits; `INSERT ... SELECT`, `UPDATE`, `DELETE` and `MERGE`, at top level and
in `CALL` and `DO` bodies, yielding on storage reads while they select and lock
rows and restarting from their start; routine calls inside the rows of an
`INSERT`, `UPDATE`, `DELETE` or `MERGE` (expressions, join conditions, row
triggers, `RETURNING`) at top level, in PL/pgSQL statements (including static
`RETURNING ... INTO`) and `EXECUTE`, and in SQL-language bodies, which keep
the rows already changed so the call sees them, as in PostgreSQL; `MERGE` as a
static PL/pgSQL statement and through `EXECUTE`; routine variables and a
trigger's `NEW` and `OLD` read as the outer level of every subquery of a
static statement (derived tables, `UPDATE ... FROM`, `MERGE` sources,
correlated, grouped and set-operation subqueries); `BEFORE` and `INSTEAD OF`
row trigger bodies that need the engine (DDL, view and rule writes,
procedures with DDL, calls reading the trigger's own writes), which run once
on it between write attempts with their result kept in the invocation log;
`FOR` loops over a data change whose `AFTER` trigger waits; `AFTER` triggers
of writes in a PL/pgSQL or SQL-language function called from a query, which
run on the engine (DDL and routine calls included) and restart the statement
on a wait; multi-step
statements nested in another's step (a rule action whose target has rules),
which keep their own steps up to `write_step_levels` levels deep.
The
[request path](docs/architecture.md#the-request-path) describes how saved state
is owned.

**Remaining:**

- **Functions:** a wait inside a write in a function called from a query restarts the statement. A top-level `SELECT` that the retained reader cannot hold (joins, sorting,
  grouping, subqueries) restarts once for each first call of a SQL-language
  routine, so its work grows with the square of those calls; single-table,
  `VALUES` and `generate_series` sources hand each call off on its row.
- **Write phases:** a write keeps its changed rows across storage and lock
  waits and continues at the unfinished row, at top level and in `CALL` and
  `DO` bodies (see the
  [execution model](docs/architecture.md#execution-model)). Remaining: a
  write whose scan ran a volatile built-in reads its rows synchronously.
  Reads after a write's last row (statement-level and `AFTER` triggers,
  foreign key checks queued as constraint events, transition tables, the
  next step's selection) read synchronously: the write completion advances
  its event cursor before an event runs, so yielding there first needs the
  completion to run the waiting event again instead of skipping it.
  `INSTEAD OF` view rows, DDL, COPY, replication apply and deferred
  triggers read synchronously. A routine call inside the rows
  keeps the changed rows (row progress), also for views and rules written
  from PL/pgSQL and SQL-language bodies. Extend row
  progress to foreign tables and to data-modifying `WITH` inside routine
  bodies: a routine call after their first row change fails with `0A000`
  (`SQL row expression requires retained write progress`), because
  restarting would hide the earlier rows from the call. Multi-step
  statements nested deeper than `write_step_levels` fail the same way with
  `54000` naming that setting. `INSERT ... SELECT` materializes its source
  before writing. Volatile user routine calls in its output list (also a
  rule action's `VALUES` over several rows) run as each row is inserted, as
  in PostgreSQL; calls in its `WHERE`, join conditions, subqueries,
  aggregate arguments or a `UNION`/`INTERSECT`/`EXCEPT` branch still run
  before the first row, so they do not see rows the statement inserts.
  Running those row by row needs the source and the row phase to keep
  separate invocation logs, since the source then runs ahead of the rows by
  a different amount in each attempt. A data-modifying `WITH` sub-statement runs to completion
  before the main statement reads it, where PostgreSQL interleaves them row
  by row, so a call in the main statement sees all of its rows. PL/pgSQL
  rejects statements that start with `WITH` (`0A000`).
- **Routine variables in `WITH`:** a `WITH` query that a static PL/pgSQL
  statement materializes before its main query (`INSERT INTO t WITH c AS
  (SELECT var) SELECT * FROM c`) cannot read routine variables (`42703`):
  `WITH` materialization takes no enclosing row. A `WITH` inside a derived
  table reads them.
- **Results and cursors:** resume more large results row by row; only
  single-table `SELECT` from a simple Query or an unpaged `Execute` does today.
  Remaining: paged and named portals (`Execute` with a row limit still buffers
  into `portal_result_bytes`), `FETCH` output, `COPY TO`, and more retained
  shapes (joins, sorting, grouping, subqueries, set operations, and more
  set-returning functions than `generate_series` and one-array `unnest`).
  Cursors over those shapes still run at `DECLARE`, so a scrollable sorted
  cursor returns saved values where PostgreSQL evaluates a volatile output
  list again, and a row-locking cursor over a join, or ordered by an
  expression that is not a source column, locks its rows at `DECLARE`.
  PL/pgSQL bound cursor declarations (`c CURSOR FOR ...` in `DECLARE`) are
  rejected with `0A000`; a `refcursor` opened with `OPEN ... FOR` works. Saving a held cursor at
  commit yields during storage waits. Cursors keep declared record and domain
  types after their source query ends.

**Done when:**

- Writing statements and statements past their first effect no longer block the
  worker on storage reads; the blocking list in the
  [request path](docs/architecture.md#execution-model) is empty except where
  durable commit order requires it.
- Resumed work keeps its selected definitions, inputs, settings, effective user,
  command visibility, timestamps and deadline. Sequence calls, advisory locks,
  audit writes and delivered rows happen once, across repeated waits.
- A failed save leaves the live call usable. Reused temporary memory cannot
  change saved text, arrays, records or NULL field types. Internal waits do not
  trigger user error handlers. Savepoint boundaries hold.
- Cancellation, timeout, rollback, disconnect and completion release saved
  state. Memory stays inside startup pools, including 4,096-row results inside
  the 4 MiB statement arena.
- Tests with an actually delayed object-store GET show another client making
  progress, with empty caches, both transaction modes, both wire protocols,
  triggers and nested functions. The same SQL gives the same rows, audit writes,
  sequence values, types, errors and rollback as PostgreSQL 18.

### Isolate readers and bound cleanup

**Goal:** a reader finishes with the rows and definitions it selected while
schema changes, commits and maintenance continue; cleanup never removes
anything a reader or backup still needs.

**Example:** a report keeps reading `orders` while another connection renames it
to `sales` and maintenance replaces its saved files. A waiting schema edit never
attaches to a new object that reused the old name.

**Remaining:**

- Catalog reads after listing an object refer to that same object: columns,
  indexes, comments and dependencies (including repeated dependency rows).
  Unfinished edits stay private. Follow PostgreSQL's split between listing an
  old catalog row and resolving a current name: an older report can still list
  `orders` while `to_regclass('sales')` finds the renamed table.
- Keep old rows, indexes and saved files until readers and backups finish, and
  remove them in bounded batches. Files from an unfinished checkpoint stay owned
  until its publication resolves. Durable manifests never name unfinished
  uploads, and switching output producers never cancels another job's files.
- Keep read counters through savepoint rollback and release. A replacement
  table reusing a slot never inherits a retired table's counters.
- Reject corrupt or stale restored state before accepting writes. A lost commit
  reply resolves to all changes or none.

**Done when:** overlapping reads, schema edits, publication and maintenance keep
readers on their selected objects and values, new reads see whole commits, and
existing rollback, savepoint, prepared-transaction, capacity and empty-cache
recovery tests keep passing.

### Measure a first deployment baseline

**Goal:** test the core bet early: commits through object storage give useful
latency and cost compared with PostgreSQL.

**Example:** on two fixed hosts with separate object storage, report typical and
slow commit latency, point-read and scan latency with warm and empty caches, and
object requests and bytes per transaction.

**Work:** record hosts, network and object storage without publishing
credentials. Run the [measurement suite](docs/performance.md) repeatedly against
both PostgreSQL 18 controls: one with the host's resources and one with the same
CPU and memory allowance as pos3ql. Retain revisions, raw results, latency
distributions, CPU/RSS, request counts, bytes and cost inputs in
[benchmarks](benchmarks/README.md).

**Done when:** a dated, reproducible baseline is published. Its results inform
the order of the later milestones.

### Run SQL on several CPU workers

**Goal:** independent connections use several CPU cores. Each connection keeps
its command order, and every confirmed transaction stays recoverable.

**Work:** choose a fixed worker pool before startup. Give each worker its own
temporary execution state and protect shared rows, definitions, locks and
transactions. Preserve database and account identity, fair queues, cancellation,
storage waits and commit order. A global lock that runs statements in turn does
not meet this goal.

**Done when:** read, write and mixed workloads scale with workers, overload
returns explicit errors, and all correctness gates pass with several workers.

### Share fixed capacity by demand

**Goal:** fixed startup pools of memory and disk follow current demand instead
of private per-table reservations.

**Example:** configuring many tables does not reserve row tracking space for each
empty one. If most traffic touches `orders`, it uses the free shared slots. A
long report keeps its selected versions until it finishes.

**Done today:** row metadata, foreground row work, table definitions, index
bindings, analyzed columns and procedural execution state share explicit pools.
[Architecture](docs/architecture.md#memory-and-physical-size) lists them.

**Remaining:**

- Reserve forward progress for commit, rollback, required reads and maintenance;
  reclaim in bounded steps; report pinned capacity and named exhaustion.
  Replacements are reserved before effects. For example, if analyzing `orders`
  cannot reserve statistics space, the previous statistics stay usable.
- Reduce the remaining catalog reservations from measured component costs.
- Decide whether cache files need guaranteed disk reservation, then implement
  and verify it. File length is not reserved disk space.
- Measure 1, 8, 32 and 128 tables at equal explicit capacity in both modes, with
  narrow and wide schemas, retained readers and schema/index replacement. Label
  every counter and unit; separate startup reservations, resident pages, stack
  and TLS. Compare with the
  [historical embedded measurements](https://github.com/e6qu/pos3ql/blob/4054a6d3c79e916728f8e84d84139eae6d86b501/benchmarks/baselines/2026-10-30-embedded-lazy-definitions/README.md)
  only at identical capacities.

**Done when:** total reserved memory is unchanged, capacity is conserved, no
runtime heap growth occurs, release after errors is safe, and the measurement
matrix supports any claimed embedded minimum. The
[resource design](docs/adaptive-resources.md) holds the proposed policies.

### Adapt batching and layout with feedback control

**Goal:** inside fixed limits, pos3ql adjusts batch sizes, admission and future
data layout to the observed workload, using measured feedback control.

**Example:** an isolated lookup starts immediately. Under sustained traffic,
compatible reads and writes share bounded batches. A nightly column report does
not evict every frequently reused lookup block.

**Work:**

- Keep fixed request slots, arenas and disk limits. Adjust active windows,
  lease ownership and batch sizes inside them. Bound queue delay and bytes.
  Protect demand reads, commit publication and required maintenance before
  read-ahead or duplicate requests.
- Compare simple feedback, proportional-integral (PI) and
  proportional-integral-derivative (PID) controllers. Each has an explicit
  policy, ceilings, minimum progress, sample interval and count, and maximum
  batch delay and bytes. Clamp changes, filter noisy samples, avoid repeated
  reversals, and stop integral error from accumulating when owned capacity
  cannot be reclaimed (anti-windup). Choose the controller from measurements,
  not by assuming a more elaborate one is better.
- Choose future row or column output, and ranged or whole-object reads, from
  measured column demand, reuse, update rate and rewrite cost. Avoid rewriting
  layout after short bursts.

**Done when:** fixed and adaptive policies are compared at equal resources on
MinIO and SeaweedFS against both PostgreSQL controls, across idle, burst,
sustained and cooling traffic, point and scan mixes, bulk writes, delays,
failures, cancellation, long readers and maintenance pressure. Results include
latency distributions, throughput, requests and bytes, useful prefetch, cache
and pinned usage, fairness and controller settling time. Correctness, durable
order and startup limits hold throughout. The
[resource design](docs/adaptive-resources.md) holds the controller design.

### Close application compatibility gaps

**Goal:** applications that use supported PostgreSQL 18 features work unchanged.

**Work:** audit PostgreSQL 18 built-in functions by signature and executed
behavior; being listed in the catalog is not support. Keep the SQLAlchemy,
Alembic, psycopg2 and other client comparisons passing, and turn every
unsupported case into an explicit error. Known gaps: `ALTER`, `COMMENT ON`
and `GRANT` on a built-in function (`ALTER FUNCTION lower(text) STABLE`) fail
with `0A000` where PostgreSQL changes the catalog entry; `information_schema`
domains such as `cardinal_number` do not exist. A SQL function whose earlier
statements are not queries or data changes (`CREATE TEMP TABLE t(a int);
SELECT a FROM t`) has its result converted value by value at the call instead
of being type-checked like PostgreSQL's analysis just before the final
statement runs. Expressions are shown as stored, not deparsed: `pg_get_expr`,
`pg_get_function_arguments` and `pg_get_functiondef` print a default `'x'` for a
`text` parameter where PostgreSQL prints `'x'::text`, `pg_get_viewdef` returns
the view's query text, and a `BEGIN ATOMIC` body is printed as written rather
than one deparsed statement per line. Errors in a client's query report
PostgreSQL's cursor position (the `P` field that psql shows as `LINE 1:` and a
caret) for syntax errors and for unknown relations, columns, types, functions
and operators, failed constant input and non-boolean conditions, except a
folded or quoted identifier in a statement that waited for typed inputs and a
constant written inside a routine body. `INSERT INTO t (a[1], b) SELECT 1` reports PostgreSQL's
`INSERT has more target columns than expressions` without its position; other
target and expression count errors are positioned. An error raised while
PostgreSQL inlines a SQL function's body
(`SQL function "f" during inlining`) has no context line. Under
`#variable_conflict use_variable` a variable that is also a column fails with
`0A000`: aggregate inputs and grouping keys bind names to columns before the
variable could win. PostgreSQL fixes a function's `plpgsql.variable_conflict`
when its creating session validates it at `CREATE FUNCTION`; pos3ql reads it
at the backend's first call of a function or top-level `CALL`, and at every
call of a trigger function or of a procedure called from PL/pgSQL. PL/pgSQL `#print_strict_params` and `#option
dump` fail with `0A000`, so `STRICT` errors carry no `parameters:` detail, and
a block label (`<<outer>>`) is rejected, so `outer.x` cannot name a block's
variable. A constant compared with a column inside a subquery of an `INSERT`, `UPDATE` or `DELETE`
(`DELETE FROM t WHERE a IN (SELECT a FROM u WHERE a = 'q')`) is checked only
when a row is compared, so an empty table succeeds where PostgreSQL rejects
`'q'`; a `SELECT`'s subqueries are checked at every level. Assigning a composite
column's field (`UPDATE t SET pair.x = 1`) is a syntax error; PostgreSQL
assigns the field. A PL/pgSQL subscripted assignment converts its element
with an explicit cast, so `x[1] := true` for an integer array stores 1 where
PostgreSQL rejects the boolean while analysing the assignment. A
data-changing `WITH` query that the main query does not read runs before it,
so a volatile function the main query calls sees its rows; PostgreSQL runs it
after the main query.
Checks PostgreSQL makes in parse analysis or execution (type modifiers, COPY
options, storage parameters, sequence, database and large-object values,
SQL/JSON result types) are reported when the statement would run. In a
`SELECT`, an unknown relation, a modifier's unknown type and an output column
written before the failing modifier are reported first, as PostgreSQL's
analysis order finds them; in other statements the modifier error comes first.
A `RETURNING` list routed through a rule's `RETURNING` list reads the rule's
relation only: `UPDATE v ... FROM o RETURNING o.label`, `*` beside `FROM` or
`USING` sources, and subqueries in that list fail with `0A000` where
PostgreSQL evaluates them.
COPY to or from a server file or program,
`COPY ... FREEZE`, a COPY `ENCODING` other than UTF8 and `JSON_QUERY ...
RETURNING` a non-JSON, non-string type are rejected with `0A000`.
A SQL-language procedure called from a trigger fired inside a write (a BEFORE
row trigger, a deferred constraint trigger at `COMMIT`) runs queries, data
changes and nested `CALL`, but rejects DDL and other utility statements with
`0A000` because that trigger runs without the engine's utility executor. A
table's or view's row type cannot
appear in a routine signature: `CREATE FUNCTION f(orders)` fails with `0A000`
where PostgreSQL accepts it. Under `READ COMMITTED`, `MERGE` and `SELECT ... FOR UPDATE` outside a cursor
that lock a row another transaction updated after their snapshot act on, or
return, the version they read; PostgreSQL re-evaluates the latest version
(EvalPlanQual), as pos3ql's `UPDATE`, `DELETE`, row-locking cursors and
foreign key checks do. The general query executor needs to re-run a locked
row's conditions and output list against the latest version, or the waiting
statement needs a fresh snapshot on every path that resumes it. The
[compatibility guide](docs/postgresql-18-compatibility.md) records the current
surface.

**Done when:** every PostgreSQL 18 built-in is either verified against
PostgreSQL or rejected explicitly, and the client suites pass on every native
target.

### Qualify production deployment

**Goal:** a user can choose hardware and trust failover from published evidence.

**Work:** extend the baseline to useful data sizes, joins, large catalogs,
maintenance, several workers, logical replicas, delays, recovery and multi-host
failover with real client routing. Run each scenario repeatedly on pinned
hardware with independent object storage and both PostgreSQL controls.

**Done when:** published measurements support stated sizes, transaction rates,
bandwidth and failover times. Calculated estimates and theoretical address
limits do not count as evidence.

## Working rules

[AGENTS.md](AGENTS.md) holds the engineering and change rules and
[CONTRIBUTING.md](CONTRIBUTING.md) holds the verification gates. The roadmap is
finished only when every milestone's "done when" is demonstrated.
