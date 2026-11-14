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
single-table and generated-series results, and paged portals over them,
resuming row by row across client, storage and lock waits; `INSERT ... SELECT`,
`UPDATE`, `DELETE` and `MERGE`, at top level and in `CALL` and `DO` bodies,
yielding on storage reads while they select and lock rows and restarting from
their start; routine calls inside the rows of an `INSERT`, `UPDATE`, `DELETE`
or `MERGE` (expressions, join conditions, row triggers, `RETURNING`) at top
level, in PL/pgSQL statements (including static `RETURNING ... INTO`) and
`EXECUTE`, and in SQL-language bodies, which keep the rows already changed so
the call sees them, as in PostgreSQL; `MERGE` as a static PL/pgSQL statement
and through `EXECUTE`;
routine variables and a trigger's `NEW` and `OLD` read as the outer level of
every subquery of a static statement (derived tables, `UPDATE ... FROM`,
`MERGE` sources, correlated, grouped and set-operation subqueries); `BEFORE` and `INSTEAD OF`
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

- **Functions:** a wait inside a write in a function called from a query
  restarts the statement. PL/pgSQL cursor reads (`OPEN`, `FETCH` and cursor
  `FOR` loops) read storage synchronously; their lock waits already park. A top-level `SELECT` that the retained reader cannot hold
  (`DISTINCT` over routine output, grouping sets, groupings whose keys or aggregate inputs call
  routines, correlated subqueries, sorts in object-store mode or over non-volatile
  routine output, joins it would probe through an index) restarts once for
  each first call of a SQL-language routine, so its work grows with the square
  of those calls; single-table, `VALUES` and `generate_series` sources,
  retained nested-loop and hash joins, local-mode sorts (volatile output runs
  after the sort), retained groupings, and cursors ordered by source columns,
  hand each call off on its row. Removing the restart needs those shapes retained (above): the
  general executor cannot run a routine body inside expression evaluation.
- **Catalog history:** a transaction that changes a catalog relation
  synthesizes that whole relation twice (its committed and its private image)
  in the statement arena to capture the changed rows. `pg_proc` history
  includes only the built-in routines that have a per-database entry, so
  `CREATE FUNCTION` is bounded by the stored and altered routines, not the
  3,000-row built-in catalog. Capturing only the rows the transaction's
  changes name would bound every relation by the change.
- **Write phases:** a write keeps its changed rows across storage and lock
  waits and continues at the unfinished row, at top level and in `CALL` and
  `DO` bodies (see the
  [execution model](docs/architecture.md#execution-model)). Remaining: a
  write whose scan ran a volatile built-in reads its rows synchronously.
  A top-level write's `AFTER` events, foreign key checks and immediate
  constraint checks yield; reads after an event's first effect block until that event ends,
  so a trigger body that writes and then reads cold data still blocks.
  Writes in `CALL` and `DO` bodies keep their `AFTER` work the same way. The
  selection of a rule action after the rule's earlier steps yields and rolls
  back to the step's start (a data-modifying `WITH` main statement after its
  sub-statements still reads synchronously); a
  statement whose restarts repeat downloads without progress reads the rest
  synchronously (`statement_restart_waits`), so it finishes with empty
  caches. The `AFTER` work of a top-level rule-rewritten write's steps
  yields, also for rule steps nested in an action's own rules: the step stays
  finished, and the statement continues with the steps after it in another
  attempt that starts both of its replay logs (routine results and saved
  call inputs, and sequence values) past the finished steps and that work.
  The `AFTER` work (foreign key checks included) of a data-modifying `WITH`
  main statement's steps, of rule steps inside a routine body, and of writes
  inside functions called from a restartable statement still reads
  synchronously: giving those a step completion needs the same step record
  in their enclosing write's attempt loop, which a routine body's write and
  the `WITH` statement's steps do not hand to it yet. A data-modifying `WITH`
  `INSERT` runs as its rows are read, as PostgreSQL's CTE scan pulls its
  ModifyTable node (`src/sql/driven_insert.rs`): each read past the rows it
  returned runs one more step, which writes source rows until one returns a
  row, so row triggers, calls and sequence values interleave with the
  reader's and `AFTER` events queue in PostgreSQL's order; queries read only
  partly, or not at all, then run to their end, last one first. The reader
  is a `SELECT` whose single `FROM` item is the query (also each query of a
  `UNION ALL` of such `SELECT`s), another such `INSERT ... SELECT`, or a
  top-level `INSERT ... SELECT` from it, with or without calls; an
  `UPDATE`, `DELETE` or `MERGE` query returns each row before it changes the
  next, as an `INSERT ... VALUES` does. A sort, aggregate or window reader
  reads its whole input first, as PostgreSQL's does. A join reader reads the
  query whole at its first read: that matches PostgreSQL when its plan
  hashes the query, but not when its plan streams the query (a nested loop
  under a `LATERAL` item over it, or a hash join that hashes the other,
  smaller input). Matching those needs a retained join node that pulls the
  query as its outer input and PostgreSQL's choice of which input to hash,
  which follows its cost estimates rather than the statement's shape.
  read. A wait inside a step runs the statement again from its first start.
  `INSTEAD OF` view rows, DDL, COPY, replication apply and deferred
  triggers read synchronously. A routine call inside the rows
  keeps the changed rows (row progress), also for views and rules written
  from PL/pgSQL and SQL-language bodies, and for foreign tables, which keep
  their selected remote rows, also beside the local rows' stored images of
  a table mixing local and foreign children. Extend row progress to
  data-modifying `WITH` inside routine bodies: a routine call after their
  first row change fails with `0A000`
  (`SQL row expression requires retained write progress`), because
  restarting would hide the earlier rows from the call. Multi-step
  statements nested deeper than `write_step_levels` fail the same way with
  `54000` naming that setting. `INSERT ... SELECT`
  streams a source that calls no volatile built-in and no user routine; one
  whose output list alone calls volatile built-ins is read by the engine row
  by row, its output evaluated as each row is read and written; one reading
  a data-modifying `WITH` query is read so too, a user routine call that
  waits inside its body keeping its log with the write and continuing
  there; any other source materializes before writing. A plain source
  calling user routines still materializes: read row by row, each call's
  results would stay in the statement arena's retained tail until the
  statement ends, so a long source would exhaust it (`54000`); streaming it needs
  per-row result storage released once the row is written. Volatile user
  routine calls in a materialized source's output
  list (also a rule action's `VALUES` over several rows) run as each row is
  inserted, as in PostgreSQL; calls in its
  `WHERE`, join conditions, subqueries, aggregate arguments or a
  `UNION`/`INTERSECT`/`EXCEPT` branch still run before the first row, so
  they do not see rows the statement inserts. A `WITH` `INSERT`
  read row by row re-reads nothing between steps: its reader and analysed
  statement stay in a 512 KiB block of the statement arena's persistent
  tail. A PL/pgSQL `WITH ... INSERT/UPDATE/DELETE` statement fails with
  `0A000`. A data-modifying `WITH` query's rules must leave exactly one
  statement after each action is rewritten again by its own target's rules,
  as PostgreSQL checks before anything runs; the `RETURNING` checks come
  first, and a rule action keeps its `RETURNING` list only when the query
  it replaces has one.
- **Routine variables in `WITH`:** the `WITH` queries of a PL/pgSQL
  `RETURN QUERY`, `SELECT ... INTO`, `PERFORM` or `FOR` query read the
  routine's variables, materialized or not, but three other `WITH` positions
  cannot (`42703`): a static `INSERT`, `UPDATE` or `DELETE` that starts with
  `WITH` (`INSERT INTO t WITH c AS MATERIALIZED (SELECT var) SELECT * FROM
  c`), a materialized `WITH` inside a derived table (`FROM (WITH c AS
  MATERIALIZED (SELECT var) SELECT * FROM c) s`), and an ordinary `WITH`
  query that a data-modifying one reads (`WITH c AS (SELECT var), w AS
  (INSERT ... SELECT * FROM c RETURNING ...)`): their expansion or run takes
  no enclosing row. Passing the routine's row to those expansions, as
  `expand_plpgsql_with` does, fixes them.
- **Results and cursors:** large results resume row by row for `SELECT`
  from one table or builtin set-returning functions with constant arguments,
  or a nested-loop or hash join of up to eight such items and `VALUES` lists
  (inner, cross, `LEFT`, and two-item `RIGHT` and `FULL`),
  with an `ORDER BY` or `DISTINCT ON` sorted in `plan_build_bytes` in local mode or a
  `GROUP BY`, `HAVING`, aggregates and plain `DISTINCT` computed there, or a
  `UNION ALL` of such queries, with inlined CTEs, views and simple FROM
  subqueries merged in, including paged portals, forward `FETCH` from `NO SCROLL` cursors and
  `COPY ... TO STDOUT` of such a query or one table. Remaining:
  - More retained shapes, by the
    [retained plan tree design](docs/architecture.md#retained-plan-tree):
    index-probed joins, `RIGHT` and `FULL` joins of more than two items or
    over generator calls, grouping sets,
    subqueries other than the simple FROM subqueries merged into their query
    and uncorrelated scalar, `EXISTS`, `ARRAY`, row and `IN`/`ANY`/`ALL`
    subqueries of unsorted, sorted or grouped streams (correlated ones,
    grouped or bounded derived tables, and scans of CTEs that run once). A
    correlated subquery binds its result type, executor, aggregate calls,
    row type or list collations once per statement, but still resolves its
    FROM scope and evaluates its operators unbound each time a row reaches
    it; rewinding a retained child tree per outer row removes that analysis,
    set operations other than `UNION ALL` (and `UNION ALL` under
    `ORDER BY`, in a declared cursor, or with volatile or routine-calling
    expressions), sorts in object-store mode (external runs
    with a run reader per stream; a large ordered `SELECT` there still
    spools, and a paged portal of one buffers in `portal_result_bytes`),
    and more
    set-returning functions than the generator builtins (series, subscripts,
    one-array `unnest`, regex, and the `json`/`jsonb` `_each`,
    `_array_elements` and `_object_keys` families, alone or in `ROWS FROM`):
    function arguments that are not constants, user-defined and other builtin
    set-returning functions, and column definition lists such as
    `jsonb_to_recordset(...) AS t(a int)`.
    A cursor over those shapes runs its query whole at its first read, with
    the snapshot taken when it opened: a scrollable cursor ordered by an
    expression that is not a source column returns saved values where
    PostgreSQL evaluates a volatile output list again, and a row-locking cursor
    over a join, or ordered by such an expression, locks all its rows at that
    first read.
    Cursors over retained joins already read as they fetch; declared cursors
    do not yet use the retained sort.
  - Scrollable cursors whose revisited rows are evaluated again (volatile
    output or filter), ordered scrollable cursors and held cursors still save
    their history in `cursor_bytes`. A scrollable cursor moving back rereads
    its input from the first row; retained position checkpoints would bound
    that reread.
  - In object-store mode a join or grouping runs in the general executor
    first and streams only once its output overflows, so a cold input larger
    than the block caches restarts on each cold read until the synchronous
    fallback reads it. Opening such a stream before it runs needs the
    retained readers to report the general executor's analysis errors
    (literal checks in join conditions, aggregates owned by an outer query,
    user functions named like aggregates, aggregates over `record`) and to
    resume their concurrent cold reads without stalling.
  - Saving a held cursor at commit reads storage synchronously: the fourteen
    callers of the commit path (statement, protocol, procedure and
    replication commits) do not yet accept a wait before the commit is
    staged.

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
unsupported case into an explicit error. Known gaps: renaming or moving a
built-in function fails with `0A000` where PostgreSQL changes the catalog
entry (every built-in name lookup must consult a renamed or moved row); `pg_type` lists all 473
`pg_catalog` types but of `information_schema`'s 148 only its domains and their
arrays (the row types of its views, whose OIDs initdb assigns after importing
the platform's collations, need the pinned reference image); a
listed built-in that is neither implemented nor an operator, cast or input
function fails with `0A000` when called; `DROP ROLE`'s `DETAIL` lists
owners by OID and privileges by grant order, where PostgreSQL lists
`pg_shdepend` rows in their physical order (insertion order in a fresh
catalog), which needs a durable stamp for each owner change;
built-in conversions and collations have no
shipped `pg_description` text and `pg_conversion` lists no built-in rows (the
other shipped descriptions are listed); `pg_type`, `pg_operator` and
`pg_opfamily` return built-in rows in OID order rather than a fresh cluster's
physical order (`pg_opclass`, `pg_amop` and `pg_amproc` match it). psql's
describe commands match PostgreSQL 18.6 except: `\dfS+` over 3,400 routines takes about twice as long as
PostgreSQL (0.5 s against 0.24 s in release builds on one host, 2026-11-13)
because each row's calls repeat overload resolution and expression typing
that PostgreSQL does once while planning. Parameter inference walks a statement in its own order rather than
PostgreSQL's left-to-right analysis, so a parameter typed by one use and passed
untyped to an `"any"` argument by another can be accepted where PostgreSQL
reports `42P18`, and a user-defined polymorphic argument left untyped becomes
`text` where PostgreSQL reports `42804`. 
A SQL function whose earlier
statements are not queries or data changes (`CREATE TEMP TABLE t(a int);
SELECT a FROM t`) has its result converted value by value at the call instead
of being type-checked like PostgreSQL's analysis just before the final
statement runs. Column defaults, CHECK constraints, routine parameter defaults
`RETURN` bodies, index predicates, trigger `WHEN` conditions and rule
conditions are printed in PostgreSQL's analyzed form (`'x'::text`,
`((k >= 1) AND (k <= 5))`), but only for literals, columns, parameters,
operators, `IN`, `LIKE`, `IS NULL`, `IS [NOT] DISTINCT FROM`, `IS [NOT]
TRUE/FALSE`, `CASE`, casts, SQL value functions, positional calls of
built-in and user-defined functions, `ROW(...)` and whole-row references;
other expressions (subqueries, named-argument calls, `$n` parameters and
array constructors outside a quantified comparison) are still printed as
stored. CHECK and exclusion constraints, column defaults and generation
expressions, and index keys and predicates are bound to the routines,
operators, types and collations they named when written (see
[stored expressions](docs/architecture.md#stored-expressions)): a later
search path, rename or schema move changes nothing, and dropping a named
object reports and cascades to them. Statistics expressions, trigger `WHEN`
conditions and domain constraints also keep routine and type names as SQL
text, so renaming or moving a routine or type one of them names fails with
`0A000`, as does renaming an enum value any stored expression casts to;
binding them follows the same dependency-list pattern. Renaming an enum value that a cast literal in a SQL-standard routine
body spells (`RETURN 'a'::e`) leaves the body spelling the old label, so the
call fails with `22P02`. A rule action that names its table's renamed column without
`NEW.` or `OLD.` fails the rename with `0A000`.
The differential corpora compare notices, warnings and INFO messages with
their statement positions. The planner folds constants as PostgreSQL's
`eval_const_expressions` does: before a `SELECT`, `INSERT ... VALUES`,
`UPDATE` or `DELETE` reads or writes a row, each constant subexpression that
calls an immutable routine, converts a value or runs an operator's routine
is evaluated once and replaced with its value, so `''::tsquery` in a query
returning three rows sends its notice once, and a constant in an arm no row
reaches still folds and reports its error. An `INSERT` folds its `VALUES`
rows and `RETURNING` list before its statement triggers fire. Folding gaps
remain: aggregate arguments, window specifications, `MERGE`,
`ON CONFLICT DO UPDATE` and `FROM` subqueries are evaluated where they run;
join conditions and `FROM` function arguments are evaluated where they run
(a constant in a join condition already raises its notice once, but a
`LATERAL` function's argument raises it for each outer row); folding them
with the target list raised the notice a second time, because the join and
function-scan executors evaluate the written conditions and arguments
again, so both need to read the folded clause; a parameter of a custom plan does not fold; a literal conversion
in a prepared statement raises its notice at each `EXECUTE` where
PostgreSQL converts it once at `PREPARE` (parse analysis), which needs the
prepared statement to keep every converted literal, not only the typed
inputs it keeps today; a statement a routine runs folds at each run where
PostgreSQL folds its cached plan once per session; and PostgreSQL's
selectivity estimate, which evaluates a stable call compared with an indexed
column once more while planning, is not modeled.
`UPDATE` and `DELETE` whose conditions (`WHERE`, `FROM` or `USING` items, or
row-security policies) call a user routine or a volatile built-in qualify
each candidate row in their change loop, just before changing it, as
PostgreSQL's executor does: the condition's notices interleave with the
row's `SET` expressions, triggers and `RETURNING` list, and its calls see the
rows the statement already changed. Candidates are every row the
statement's command snapshot sees (through the condition's index when one
answers it), so rows this command changed are never revisited, and each
qualified row is locked there, with read committed's recheck of a row
changed while waiting. Row progress records the candidate position, so a
write resumed after a routine call or a wait neither qualifies earlier
candidates again, whether they changed or failed, nor repeats their calls;
an uncorrelated subquery the statement keeps (an initPlan) replays at the
log position where it first ran. Conditions without such calls select and
lock every row first, which no client can tell apart. Remaining gaps: a
foreign table's rows are qualified before any change; PostgreSQL orders a
condition's conjuncts by cost and evaluates leakproof ones before
row-security conditions (`id < 4 AND f(id)` runs `id < 4` first), where
conditions here run row-security policies first, then the condition left to
right; a data-modifying `WITH` sub-statement runs ahead of the main
statement (above); and a rule action's `NEW` row evaluates every `SET`
expression of the original statement, where PostgreSQL's rewrite evaluates
only those the action reads. A policy that a write's `UPDATE` (or `DELETE`)
and `SELECT` commands share runs once per row, as PostgreSQL's
`list_append_unique` keeps it; each command's restrictive policies run
before the OR of its permissive ones, which stops at the first that passes.
A row no partition accepts reports its partition key
(`Partition key of the failing row contains (id) = (21).`); like the key
details of unique violations, it is not withheld from a role without
`SELECT` privilege or under row security as PostgreSQL withholds it.
Row order
without `ORDER BY` follows a model of PostgreSQL's access path rather than
its cost-based plan: a query or data change on one table whose condition
constrains an index's leading column takes rows in their first-insertion
order, as an index or bitmap scan reaches an updated row at its first
position, and any other takes them in physical order. A table with saved row
files read without index candidates keeps physical order.
An exclusion constraint element may be a column, a function call or a
parenthesized expression (`EXCLUDE USING gist (int4range(a, b) WITH &&)`).
Its elements' expressions together hold at most 256 bytes of stored text;
more fails with `54000`. An element may name any operator class of its
access method that accepts its type, built-in (`text_pattern_ops`) or a
user-defined btree class, a `COLLATE`, and, under btree, `DESC` and `NULLS
FIRST/LAST`; its operator may be user-defined when the class's family holds
it, and runs through the operator's function. Operator class options on an
element fail with `0A000`. `ALTER COLUMN ... TYPE` keeps the
OIDs of the key and exclusion indexes over the column, where PostgreSQL
rebuilds them with new OIDs, so after it a row that violates two
constraints can report the other one first. `DROP COLUMN` of a column an
exclusion expression or predicate reads lists those constraints in their own
`2BP01` report, separately from views, policies and routines that also depend
on the column, and reports `cannot drop column ...` where PostgreSQL, when
inheritance children drop the column too, reports `cannot drop desired
object(s) ...`. `ON CONFLICT DO NOTHING` without a target refuses a table
with a deferrable unique or exclusion constraint with `55000` even for a row
an earlier arbiter would skip. An inference target's `WHERE` (`ON CONFLICT
(a) WHERE a > 5`) selects a partial unique index through the same
comparison-implication test the planner uses for partial index scans, which
proves less than PostgreSQL's `predicate_implied_by` (no `OR`, `IN` or
non-integer constant ranges). `pg_trigger` omits the internal
`PK_ConstraintTrigger_*` and `Unique_ConstraintTrigger_*` rows of deferrable
unique and exclusion keys; their rechecks do fire among the row's AFTER
triggers in that name order. A statement's own catalog scans, including those through views, read the
catalog as the statement began (images built at its start), so `SELECT mk(g),
(SELECT count(*) FROM pg_class WHERE relname LIKE 'made_%') FROM
generate_series(1, 3) g`, where `mk` creates a table, counts none, as
PostgreSQL's statement snapshot does, and so does a data change whose
trigger creates tables; a volatile routine's queries see the change. A
STABLE or IMMUTABLE SQL function reads the catalog under its calling
statement's snapshot too: the images its body reads, through nested
non-volatile SQL functions, are built when the statement starts. `pg_get_viewdef`, `pg_views`,
`pg_get_ruledef` and `pg_rules` print every view's and rule's query as
PostgreSQL deparses it, in its indented layout and wrap column, from the
stored query, never its stored text: subqueries, functions (including ones
returning composite rows, `OUT` parameters or a `json_to_record` column
definition list), `JSON_TABLE` items, catalog and `information_schema` relations, joins, set
operations, windows, grouping sets, `WITH` queries with `SEARCH` and `CYCLE`,
constant and repeated sort keys, and the analyzed form of each expression
(operator coercions, SQL-syntax functions, `IN` lists, row comparisons,
`BETWEEN`, `LIKE ... ESCAPE`, `SIMILAR TO`, subscripts, `COLLATE`, typmod
casts, variadic and named arguments). Routines, types and relations are
printed under their current names at every query level. A rule on a view
prints like one on a table. A query shape the deparser does not cover fails
with `0A000` (`deparsing the query of view "v" is not supported`): a rule
action with a data-modifying `WITH` query is one. A `FROM` function returning
`record` with a column definition list is accepted only for the
`json[b]_to_record[set]` family; for other functions the list is a syntax
error, and `SYSTEM_USER` and system columns such as `xmin` are rejected.
In a `BEGIN ATOMIC` body only a `SELECT` of expressions
without `FROM` is deparsed (`SELECT (x + 1);`, under the current names of
the routines and types it named); other statements are printed one per line
as written, keeping names that were since renamed, where PostgreSQL prints its analyzed form (`INSERT
INTO t (a, b) VALUES (f.x, 'q'::text);`); policy
expressions are not deparsed. `EXPLAIN` prints
the left-deep join tree pos3ql runs, with scan filters, a hash join's
condition and each nested loop's join filter, sort and grouping keys
(`Sort Key`, `Group Key`), a `VALUES` list of several rows as `Values Scan
on "*VALUES*"`, a derived table's own plan under its `Subquery Scan` (or in
its place when the scan neither filters nor reshapes rows), and each outer
join narrowed by conditions that reject its nullable side's NULLs, as
PostgreSQL's `reduce_outer_joins` does. A derived table's, a function's
(`Function Scan`) and a pulled-up `VALUES` list's scan print their `Filter`, a
grouped query its `HAVING` filter, an `OFFSET` its `Limit`, a distinct union the
`Append` it hashes, and a relation a set operation reads again a unique name
(`t1_1`). `VERBOSE` prints relation schemas, a single relation's conditions
qualified, and a `VALUES` list's columns as `"*VALUES*".column1`; other nodes'
`Output` lists stay unqualified, join conditions over a `VALUES` list or another
derived relation are not printed, and `Sort Key`/`Group Key` items reading a
derived table are not printed (PostgreSQL prints the expressions of the
derived table's query); keys reading a function or a `VALUES` list are. An index scan prints the
conditions its index consumes as `Index Cond` (indexed column first and
unqualified), and a parameterized inner index scan filters the other join
conditions reading it, as PostgreSQL does. Outer joins of stored tables in
FROM order place an `ON` condition reading only the nullable side below the
join, keep the rest as the join's hash condition or join filter, and filter
the join's output (`Filter`) by a `WHERE` condition reading a nulled
relation. An outer hash join hashes its
nullable side (a `RIGHT JOIN` shows as `Hash Left Join` probing its right
item, as PostgreSQL plans tables of similar size), or its preserved side
(`Hash Right Join`) when only that side fits the build; a `FULL JOIN` shows
as `Hash Full Join`. Its `WHERE` conditions reading only the side it never
null-extends filter that side's scan, and its `ON` conditions reading only
the null-extended side filter that side's scan before the build, as
PostgreSQL places them; `enable_hashjoin = off` runs each as a nested loop. Other
right or full joins run, and print, as a `Nested Loop Right Join` or
`Nested Loop Full Join`, where PostgreSQL may also merge. The join order
is pos3ql's (FROM order, or its selectivity order for comma lists), which
can differ from PostgreSQL's cost-based order. The executor has no merge
join, so where PostgreSQL's costs pick `Merge Join` over sorted inputs
pos3ql shows the hash join or nested loop it runs. `UPDATE ... FROM` and `DELETE ...
USING` hash their first FROM table when it is joined to the target by
equality, as a `SELECT` of the two tables would, and join later FROM items to
it in nested loops; PostgreSQL may instead hash the joined FROM items. A statement
rewrite rules change is explained as every query PostgreSQL's rewriter
produces, each rule action rewritten again by its target's rules; a rule
action of several `VALUES` rows over a statement's query, several rows or
condition fails with `0A000` (PostgreSQL joins the list into the action as a
`VALUES` scan reading the other rows laterally). A view an `UPDATE ... FROM`
or `DELETE ... USING` reads is pulled up only when its relations' names do
not clash with the statement's own, where PostgreSQL renames them (`t_1`);
otherwise its query runs under a `Subquery Scan`. Under `EXPLAIN ANALYZE` a rewritten statement runs
once, each of its queries counted in its own plan, and its time and I/O are
reported on its last query. `EXPLAIN` in PL/pgSQL explains rewritten
statements the same way. `EXPLAIN ANALYZE`
counts, per plan node, the loops, rows and the rows its `Filter` and `Join
Filter` removed while the statement runs, and prints `(never executed)` for
nodes that did not run: scans (also a nested loop's inner rescans and
parameterized index scans), nested-loop and hash joins with their `Hash` and
`Materialize` inputs, aggregation, sorts, `DISTINCT`, limits, window
evaluation, set operations, derived tables, `VALUES` lists, a write's target
scan, an `INSERT`'s source query or `VALUES` rows, and a query without FROM
(its one-row `Result`, with a non-constant `WHERE` as its `One-Time Filter`,
and the `ProjectSet` that expands set-returning calls, which also appears
over scans), in all four formats. A hash join's `Hash` node reports the table pos3ql
built: its bucket count (and the count it started with when it grew), one
batch, and the bytes of its arrays as `Memory Usage`, which differ from
PostgreSQL's allocator figures. A query without FROM drops sort, grouping
and distinct keys that are planner constants, as PostgreSQL does (a constant
`DISTINCT` becomes a `Limit`). Conditions print coerced numeric literals as
the constants the planner folds them into (`'2'::double precision`). Times are measured
per loop, to the first row and to the end; in pos3ql's push-based executor a
node's time also includes the work of the nodes reading its rows. A locking
clause plans a `LockRows` node below the `Limit`, which counts the rows it
locked. An analyzed sort prints PostgreSQL's `Sort Method` (`quicksort`, or
`top-N heapsort` when a limit directly above bounds it and it read more than
twice the bound, which pos3ql then also keeps to a heap of the bounded rows;
`external merge` when its rows spilled to disk) and the space of the rows it
held in pos3ql's own encoding, which differs from PostgreSQL's allocator
figures. Each window clause plans its own `WindowAgg` in PostgreSQL's
evaluation order, named `w1`, `w2`, ... when unnamed and printing its
`Window:` definition (with the `ROWS UNBOUNDED PRECEDING` frame PostgreSQL
gives windows of frame-independent functions), over a `Sort` of its input
or an `Incremental Sort` when the rows already follow a prefix of its keys;
a query `ORDER BY` the last window's rows follow needs no sort. Under
`ANALYZE` a window prints its `Storage:` line, its bytes pos3ql's own. A
condition folded to false keeps the query's sort, limit, row locks,
aggregation and windows over a `Result` with a false `One-Time Filter`. A
subquery the planner does not merge runs as a subplan: an uncorrelated
scalar, `EXISTS` or `ARRAY` subquery as `InitPlan n` under the query's top
node (referenced as `(InitPlan n).col1`, and gating the scan from a `Result`
when a condition reads only such subplans), a correlated one in the select
list or `WHERE` of a one-relation query as `SubPlan n` after the inputs of the
node evaluating it, numbered as PostgreSQL plans them, with the subplan's
relations named uniquely (`u_1`). Not yet shown: `IN`/`ANY`/`ALL`/row
subqueries as subplans, a correlated subquery of a join, the `Result` that
PostgreSQL adds above a sorted, limited query to run select-list subplans
after the sort, a subplan's figures under `ANALYZE` (its nodes print none:
execution counts every subquery's operators in one block), `CTE` InitPlans
and `CTE Scan` nodes for materialized `WITH` queries (a `WITH` query is
merged into the statement, under a generated name when its relation's name
clashes), semi and anti joins for `EXISTS`, `NOT EXISTS` and `IN` (pos3ql runs
them as per-row subqueries), an incremental sort's `ANALYZE` group lines,
`VERBOSE` output lists of windows and expressions (the output columns' names
are printed), aggregate and `Materialize` storage lines, per-node `Buffers`
and `WAL` (reported on the top node only), trigger time lines, an index-only
scan's `Heap Fetches` and a bitmap heap scan's `Heap Blocks`. A node of a path
the executor does not count (a data-modifying `WITH`, a joined `UPDATE` or
`DELETE`, `MERGE`) prints no actual figures, except the top node, which reports
the statement's rows; `MERGE` plans its target scan alone where PostgreSQL
joins the target and source (`Hash Right Join`); a `LIMIT` that stops `Unique`
early leaves its `Sort` without figures, as pos3ql deduplicates before
sorting; a `LIMIT` over a locking query without `ORDER BY` reads every row
where PostgreSQL's `LockRows` stops early. Where pos3ql's plan differs from
PostgreSQL's cost-based one the figures are those of the plan it shows and
runs: it hashes the second of two joined relations where PostgreSQL may hash
the smaller filtered one, joins three or more relations in nested loops where
PostgreSQL may hash them, sorts before `DISTINCT ... ORDER BY` (and a
distinct union) where PostgreSQL may hash first, plans a grouped query whose
condition is false with `HashAggregate` where PostgreSQL may sort, and does
not flatten a distinct union of more than two inputs into one `Append`. A
`top-N heapsort` of input that spills before the sort ends is reported as
`external merge`. Notices carry
no position (`''::tsquery` points at the literal in PostgreSQL), and a
literal conversion's notice is raised where the statement folds rather than
during parse analysis, so `UPDATE t SET a = 1/0 WHERE ''::tsquery IS NOT
NULL` reports the division without the notice PostgreSQL sends first.
`EXPLAIN` plans the statement the planner's constant folding produced:
conditions print folded values (`Filter: (a > 2)`), a comparison with a NULL
constant folds to NULL, a condition folded to false or NULL leaves a `Result`
with a false `One-Time Filter` (also under `UPDATE` and `DELETE`), a
conjunct folded to true is dropped, an `IN` list of constants prints as one
array constant, and a condition that reads no column and calls no volatile
function gates a single relation's scan from a `Result`. Plain `EXPLAIN`
raises the folding's notices and errors; `EXPLAIN ANALYZE` raises them once.
Set operation leaves, derived tables and subqueries print their constants
as written. `EXPLAIN ANALYZE` runs its statement on the resumable executor,
so statements calling PL/pgSQL functions are analyzed; a write that
qualifies its rows in its change loop counts its scan's kept and removed
rows across the attempts its calls restart. A write's top node returns its
`RETURNING` rows. Errors in a client's query report
PostgreSQL's cursor position (the `P` field that psql shows as `LINE 1:` and a
caret) for syntax errors and for unknown relations, columns, types, functions
and operators, failed constant input and non-boolean conditions, except a
folded or quoted identifier in a statement that waited for typed inputs and a
constant written inside a routine body. Target and expression count errors of an `INSERT`, also with subscripted
targets (`INSERT INTO t (a[1], b) VALUES (1)`), are positioned and come
before type errors, as PostgreSQL checks them. PL/pgSQL statements, writes, `EXECUTE` and FOR queries run their `WITH`
queries, including data-modifying ones, in one new command as PostgreSQL
does; a cursor's query with data-modifying `WITH` queries runs at its first
read. Remaining: a PL/pgSQL write with `WITH` queries whose nested AFTER
trigger waits fails with `0A000` instead of resuming; and a data-modifying
`WITH` query read by a join runs whole at its first read, so where
PostgreSQL's plan streams the query (a `LATERAL` item over it, or a join
that hashes the other input) a trigger's or called function's effects
arrive in a different order than PostgreSQL's (see Write phases). The index behind a primary key or unique
constraint cannot be named by `CLUSTER ON`, `CLUSTER ... USING` or `REPLICA
IDENTITY USING INDEX` (`index "t_pkey" does not exist`). A view's
`reloptions` lists its options in a fixed order (`security_invoker`,
`security_barrier`, `check_option`) where PostgreSQL keeps the order they were
set, and omits an explicit `security_invoker=false`. A `CHECK` constraint
marked `NO INHERIT` fails with `0A000`.
`pg_constraint` omits the system catalogs' own not-null constraints.
The built-in `postgres_fdw` extension's routines cannot be called directly
(`0A000`): `postgres_fdw_get_connections` and `postgres_fdw_disconnect` need
connections cached past their transaction, which pos3ql closes at transaction
end, and `postgres_fdw_validator` is run only by wrapper DDL. A foreign
server's error keeps its SQLSTATE, message, `DETAIL`, `HINT` and `CONTEXT`
and adds `remote SQL command: ...`, as postgres_fdw reports it; the remote
command differs from postgres_fdw's own (an `INSERT` reads back
`RETURNING ctid::text` and every column, and a scan reads the whole table
with one plain `SELECT` where postgres_fdw declares a cursor and fetches
`fetch_size` rows at a time), and `batch_size` is not honored. `CREATE
EXTENSION postgres_fdw SCHEMA pg_catalog` fails because `pg_catalog` is a
read-only virtual schema. A foreign key that references a partitioned table
has only its own `pg_constraint` row: PostgreSQL also lists one clone per
partition of the referenced table (`pc_a_fkey_1`, whose `conindid` is that
partition's index). `ALTER COLUMN ... TYPE` on a partitioned table keeps
each partition's index name where PostgreSQL rebuilds them under new
generated names (`pr_1_label_idx` after a rename). A btree index may use
`text_pattern_ops`, `varchar_pattern_ops` or `bpchar_pattern_ops`, which
order and compare strings byte by byte; the planner lets such a key serve
`=` and no collation-aware comparison, and a default key no `~<~`-family
operator, as PostgreSQL's operator families do. It does not yet turn a
`LIKE 'ab%'` or `~ '^ab'` prefix into PostgreSQL's `~>=~`/`~<~` (or, under
`C`, `>=`/`<`) index range with the pattern kept as a filter, nor scan a
pattern-class key for an explicit `~<~`-family range: those keys get their
own physical binding, whose range plans need PostgreSQL's prefix and
histogram selectivity to choose its bitmap scan; `=` on a pattern-class key
shows `Index Scan` where PostgreSQL shows `Index Only Scan`. A routine parameter or result written with `%TYPE` may name a
table column but not a view column (`0A000`). A PL/pgSQL assignment to a
three-part name (`x.y.z := 1`, a labelled row variable's field) fails with
`0A000`, where PostgreSQL assigns `label.row.field` or reports `"x.y.z" is
not a known variable`. A literal passed to a function's `regclass`
argument (`nextval('nope')`) is converted when the call runs, so its error has
no cursor position. A derived table computes output
columns its query never reads, so `SELECT * FROM t WHERE EXISTS (SELECT 1
FROM (SELECT 10 / t.x) s)` fails with division by zero on a zero `x` where
PostgreSQL, which drops unread subquery outputs, returns the row. A
whole-row reference to a table (`SELECT t FROM t`) is typed `record`, where
PostgreSQL types it as the table's row type `t`, so comparing one with an
untyped literal (`WHERE t = '(1,a)'`) fails with `42883` where PostgreSQL
reports `0A000`; whole-row index keys (`CREATE INDEX ON t ((t))`) do have the
row type. A row constructor casts to a table's row type
(`ROW(1, 'a')::t`) but not to a view's, and an array of a table's row type
(`ARRAY[ROW(1, 'a')::t]`) fails with `0A000`, where PostgreSQL accepts both.
A PL/pgSQL variable declared with a view's row type (`r v;` or
`r v%ROWTYPE`) fails with `0A000`; a table's works. A `CONSTANT` assignment
error has no cursor position: PL/pgSQL assignment targets do not keep their
place in the routine body. A trigger fired inside a writing statement cannot
`FETCH` from a session cursor that calls a user-defined routine for each
fetched row (`0A000`). A protocol-level `Parse` message checks only
syntax and parameter types, so analysis errors (`SELECT current_date + $1`,
`SELECT 1 WHERE 'x'`) arrive at `Bind` or `Execute` where PostgreSQL reports
them at `Parse`, and `pg_prepared_statements` lists only SQL `PREPARE`
statements, not protocol ones.
Checks PostgreSQL makes in parse analysis or execution (type modifiers, COPY
options, storage parameters, sequence, database and large-object values,
SQL/JSON result types) are reported when the statement would run. In a
`SELECT`, an unknown relation, a modifier's unknown type and an output column
written before the failing modifier are reported first, as PostgreSQL's
analysis order finds them; in other statements the modifier error comes first.
Writes to catalog views other than `pg_settings`, and to system catalogs,
fail with `42P01`.
A subquery reading `old` or `new` in the `RETURNING` list of a write through
a view with an `INSTEAD OF` trigger or a rule (`UPDATE v ... RETURNING
(SELECT old.label)`) fails with `42P01`: analysis of the view write resolves
the subquery's outer references without the list's `old` and `new` names.
Column-not-found errors carry no `Perhaps you meant` hint.
An error inside a statement or expression of a PL/pgSQL body is positioned
in it (`QUERY` and `LINE`), except at a name the lexer folded (`OLD.n`,
`NOSUCH`) or a `$n` beyond the routine's parameters, which report no
position: the statement's parsed names keep no map to the text. A rule
created inside one (`CREATE RULE ... WHERE OLD.nosuch > 0`) also words its
error `column "nosuch" does not exist` where PostgreSQL says `column
old.nosuch does not exist`. A `$n` beyond the routine's parameters in a
statement that reads no rows (`PERFORM $1 FROM empty_table` in a `DO`
body) is accepted: it fails only when evaluated, where PostgreSQL's
analysis rejects it.
A field of an anonymous row passed to a PL/pgSQL `record` parameter that the
row lacks (`f((1, 'y'))` reading `r.a`) fails with `42P01` (`missing
FROM-clause entry for table "r"`) where PostgreSQL reports `record "r" has no
field "a"`.
`PREPARE p(t1) AS UPDATE t SET c = $1` with a parameter type the column
cannot take is accepted until it runs, and the error it then reports (also
from a cascaded `ON UPDATE CASCADE` between two composite types) has no
position: parameters carry no source position for assignment analysis.
COPY to or from a server file or program,
`COPY ... FREEZE` and a COPY `ENCODING` other than UTF8 are rejected with
`0A000`.
`COPY FROM` into a view with an `INSTEAD OF INSERT` trigger fails with
`0A000` where PostgreSQL inserts each row through the trigger, and a refused
`COPY FROM` into a view or sequence does not validate its options before
copy-in. A scalar function used as a `FROM` item (`FROM t, nextval('s') x`)
fails with `0A000` `table function "nextval" is not supported`.
A view
dropped by the cascade of a function, sequence, collation, operator, text
search object or type (`DROP FUNCTION f CASCADE` where view `v` calls `f`)
fails with `0A000` while another table's column, a composite attribute or a
domain uses the view's row type; PostgreSQL drops those users too.
`ALTER COLUMN ... TYPE` is refused for columns that a
view, rule, trigger `WHEN` condition, policy or SQL-standard routine body
reads or a stored `INSERT` or `UPDATE` writes (including a materialized
view). `INSERT INTO t VALUES (1) UNION SELECT ...` fails with `42601` where
PostgreSQL accepts a set operation over `VALUES` as the source.
`DROP` of casts, operator
classes and families, extensions, policies, triggers, rules, indexes and
`DROP OWNED` still describe their dependents with their own code instead of
the `pg_depend` walk that `DROP TABLE`, `VIEW`, `MATERIALIZED VIEW`,
`SEQUENCE`, `FUNCTION`, `TYPE`, `DOMAIN`, `SCHEMA`, `OPERATOR`, `COLLATION`,
`SERVER`, `FOREIGN DATA WRAPPER` and text search objects share, and
`pg_event_trigger_dropped_objects` describes each statement's own objects
and orders them by the walk only for the statements on it. A TOAST table
shares its table's creation stamp, so it is listed just after the table's
array type rather than after its `NOT NULL` constraints, and `ALTER TABLE ...
DROP COLUMN ... CASCADE` lists the column after its dependents instead of
first. `pg_rewrite` lists no rules of built-in views (PostgreSQL has 145
`_RETURN` rules, at OID `reltype + 1`, and `pg_settings_u`/`pg_settings_n`)
and has no `ev_qual` or `ev_action` column (selecting one fails with `42703`);
they would carry the rule's condition and actions as `pos3ql-catalog-expression-v1`
nodes, derived from the retained rule body so that the nine-field
`pg_rewrite` image keeps its shape,
and `ALTER VIEW` and `ALTER MATERIALIZED VIEW` accept only their own actions,
so `ALTER VIEW v DISABLE RULE r` fails with `42601` where PostgreSQL reports
`42809` `ALTER action DISABLE RULE cannot be performed on relation "v"`. A partition key that is an
expression (`PARTITION BY LIST ((k + v))`, `PARTITION BY RANGE (lower(a))`)
fails with `0A000`; PostgreSQL routes rows by the evaluated key and lists it
in `pg_partitioned_table.partexprs`. A table's or view's row type cannot
appear in a routine signature: `CREATE FUNCTION f(orders)` fails with `0A000`
where PostgreSQL accepts it. Under `READ COMMITTED`, a locked row's recheck
fails with `0A000` for a simple `LATERAL` subquery that reads tables when
several of its source rows produced the values the statement read but
recheck differently, or when it is a `UNION ALL` branch that reads tables
(PostgreSQL keeps the rows it first joined). A parameter used inside a
`FROM` subquery of `PREPARE` (`PREPARE p AS SELECT * FROM (SELECT 1 + $1) s`)
is typed `text` before the subquery is described, so the statement fails
with `42883`. A subquery's `t.*` naming an
enclosing query's item (`FROM t, LATERAL (SELECT t.*) s`, `SELECT (SELECT
t.*) FROM t`) fails with `42601` or `42P01`; PostgreSQL expands the outer
row's columns. The
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
