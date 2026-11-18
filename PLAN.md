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
  3,000-row built-in catalog. The synthesis walks each table's indexes by
  its slot, and collects indexes and foreign keys in one walk, so a commit
  no longer looks every table up by name for each table; checkpoint
  maintenance reads a value index binding's fields in place instead of
  copying it. `pg_attribute` looks up a foreign table's binding only for a
  foreign table, and keying a catalog's rows reuses one row-wide buffer
  (`configured_transaction_capacity_covers_truncate_fanout_and_prepared_
  cold_recovery`, which commits 300 tables' DDL, took 122 s in a local
  debug build before and 95 s after). A commit's cost still grows with the
  catalog: about four fifths of it is synthesizing each changed catalog
  relation (`pg_depend`, `pg_attribute` and `pg_constraint` most), so
  capturing only the rows the transaction's changes name, with each
  synthesizer filtered to the touched relations and their key and
  inheritance neighbours, would bound it by the change. Each table's index
  lookup still scans every index, and each foreign key finds its parent by
  name; in that commit profile they cost under five percent.
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
    In object-store mode a `LATERAL` builtin call streams its rows through
    an external run only for the functions evaluated one row at a time (the
    generator, regex and `json`/`jsonb` families, `aclexplode`,
    `pg_listening_channels`, `pg_available_extensions`); the others, such as
    the `pg_partition_ancestors` call psql's `\d` makes, materialize in the
    work arena as they do without object storage.
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
`text` where PostgreSQL reports `42804`. `DROP SUBSCRIPTION` drops the
publisher slot before it returns, as PostgreSQL does, and keeps the apply
worker stopped until its transaction ends; a synchronization slot an
interrupted initial copy left on the publisher is dropped before the copy
starts again, as PostgreSQL's table synchronization does. `CREATE
SUBSCRIPTION` with `connect = true` connects while it runs: it fails with
PostgreSQL's `08006` when the publisher is unreachable, warns about
publications the publisher lacks (`42704`), requires each published table
locally (`42P01`), creates the slot with `SNAPSHOT 'nothing'` and reports
`NOTICE: created replication slot "s" on publisher`; with `create_slot` it is
refused in a transaction block or function (`25001`). Its tables are recorded
in `pg_subscription_rel` at commit as `i` (`r` without `copy_data`)
(`differential_exact/782`). Remaining: the initial copy still runs as one
bootstrap under one synchronization slot, so a table moves from `i` to `r`
when the whole copy commits, without PostgreSQL's per-table `d`, `f` and `s`
states. A table its publications publish with different column lists fails
`CREATE SUBSCRIPTION` with PostgreSQL's `0A000` before any slot is created,
as `fetch_table_list` does; `ALTER SUBSCRIPTION ... REFRESH PUBLICATION`
still rejects it in its copy rather than in the statement. The publisher connection
needs a port, user, dbname and sslmode (`22023` otherwise), where libpq
applies its defaults, and a numeric host: resolving a host name as libpq does
is blocked by the fixed-memory rule ([B-351](BUGS.md)).
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
A `WITH` query run once is read lazily from the store its readers share,
as PostgreSQL's `CTE Scan` reads it, when the reader is its query's only
`FROM` item (`differential_exact/840`). Remaining: a join reads the whole
query at its first read (which needs the join executor below to pull its
inputs), a declared cursor runs its whole query at its first `FETCH`, a
reader that needs rows past an earlier read's stop runs the query again from
its first row (refused with `0A000` for a volatile query, which PostgreSQL
runs once), and a materialized `WITH` inside a subquery cannot read the
enclosing query's columns (`42P01` where PostgreSQL runs it as a correlated
plan). Under `EXPLAIN ANALYZE`, a subplan reading a materialized `WITH`
query of its enclosing statement prints `(never executed)` for its nodes:
the executed subquery is a copy of the explained one, so its counters key
by another identity.
The differential corpora compare notices, warnings and INFO messages with
their statement positions. The planner folds constants as PostgreSQL's
`eval_const_expressions` does: before a `SELECT`, `INSERT ... VALUES`,
`UPDATE` or `DELETE` reads or writes a row, each constant subexpression that
calls an immutable routine, converts a value or runs an operator's routine
is evaluated once and replaced with its value, so `''::tsquery` in a query
returning three rows sends its notice once, and a constant in an arm no row
reaches still folds and reports its error. An `INSERT` folds its `VALUES`
rows and `RETURNING` list before its statement triggers fire. A `SELECT`
folds its join conditions (after the target list, before `WHERE`) and the
arguments of each function a `LATERAL` item calls for every row to its
left, and its join and function scans read that folded clause; a function
scanned once is evaluated when the scope materializes it. `ON CONFLICT DO
UPDATE` folds its assignments and condition after `RETURNING`, and `MERGE`
folds `RETURNING`, each action's assignments or values and condition, then
its join condition, before its statement triggers fire. Folding gaps
remain: aggregate arguments, window specifications, `FROM` subqueries and a
`MERGE` into a view are evaluated where they run; join conditions inside a
parenthesized join are not folded; a parameter of a custom plan does not fold; a statement a routine runs folds at each run where
PostgreSQL folds its cached plan once per session; and PostgreSQL's
selectivity estimate, which evaluates a stable call compared with an indexed
column once more while planning, is not modeled.
`PREPARE` converts each literal as PostgreSQL's parse analysis does: a
literal cast to a built-in type (`''::tsquery`, `int4 '7'`, `'{1}'::int[]`)
and an untyped literal the analysis converts (an inserted or assigned value,
a comparison operand, a built-in routine's argument) run their input
function once there, raising its notice or error, and the prepared
statement keeps the converted values (with its text, in its
`prepared_bytes` slot; more fails with `54000`), which each `EXECUTE` reads.
A cast's length coercion (`'123.45'::numeric(3,1)`) and constant folding
(`1/0`) run when the statement is planned, at each `EXECUTE`. A query or data
change the extended protocol's Parse prepares is analyzed the same way at
Parse: its literal input notices and errors come back from Parse, and Bind,
Describe and each Execute read the converted values; a declared parameter type
beyond the highest `$n` the text uses still declares a parameter. Other
statements Parse prepares (utilities, `EXPLAIN`, `CREATE TABLE AS`, `CALL`,
`DECLARE`) convert their literals when they run, where PostgreSQL's parse
analysis converts those of `EXPLAIN`, `CREATE TABLE AS`, `CALL` and `DECLARE`
at Parse. A literal's input notice or error carries the literal's cursor
position, as PostgreSQL's parser position callback reports it, when the
literal is cast, assigned, compared or passed to a full-text routine.
Remaining: a simple query converts an untyped literal compared with a column
(`WHERE q = ''`) or an operator operand for each row it reads, so its notice
repeats per row where PostgreSQL converts it once while analysing; a literal
of an `INSERT ... SELECT` output or an `ARRAY[...]` element cast as a whole,
and a statement a PL/pgSQL routine runs, report no cursor position.
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
lock every row first, which no client can tell apart. A condition's
top-level conjuncts run in PostgreSQL's `order_qual_clauses` order: by
per-row cost, stably, where a routine or a user-defined operator's function
costs its declared `COST` (100 by default) times an operator, so
`f(id) AND id < 4` runs `id < 4` first, in queries, `UPDATE`, `DELETE` and
their `EXPLAIN` filters. An uncorrelated scalar, `EXISTS` or `ARRAY`
subquery is an initPlan that costs nothing per row, and an uncorrelated
`IN` or `ANY` below the top level a hashed subPlan that costs its
comparison; a top-level `IN (SELECT ...)` becomes a semi-join that runs
after the relation's own conditions, except that a write runs it first
when an index leads with its column, as PostgreSQL's plan then reads the
subquery's rows and probes the index. Under row security each policy condition runs
before the statement's conditions, except that a leakproof conjunct of a
single-relation query or write cheaper than ten operators runs before the
first policy condition when it costs less, and right after it otherwise,
as PostgreSQL's `order_qual_clauses` sorts by security level and cost; a
write's WITH CHECK options test the OR of the permissive policies before
each restrictive one. `EXPLAIN` of a single-relation query, `UPDATE` or
`DELETE` prints the policy conditions in its scan's `Filter` in that order: a
write's own policies, then the `SELECT` policies when it reads the target's
columns, each listed once. `EXPLAIN VERBOSE` of an `UPDATE` or `DELETE` of a
plain table prints its scan's `Output` (each assigned column's new value, then
`ctid`) and its `RETURNING` list, with a `DEFAULT` as the column's default
(folded when constant, a typed null without one), a constant folded to the
column's type and an element or slice assignment as `column[...] := value`;
an assignment through a composite field leaves the scan's `Output`
unprinted. A foreign table's candidate
rows are read again by their remote row identity and qualified in the
change loop the same way. Remaining gaps: an `UPDATE ... FROM` or `DELETE
... USING` whose target reaches a foreign table's rows qualifies them
before any change; `UPDATE ... FROM` and
`DELETE ... USING` cost a routine reading a joined item's column as one
operator; a data-modifying `WITH` sub-statement runs ahead of the main
statement (above). A rule action reads the triggering statement's
expression for each `OLD` or `NEW` reference it (or its rule's condition)
makes, as PostgreSQL's rewriter substitutes them: it evaluates only the
`SET` expressions, single `VALUES` row expressions and column defaults it
reads, once per reference, while an `INSERT` reading a query or several
`VALUES` rows evaluates each input column once per row. An action reading
only those expressions evaluates them row by row, interleaved with its own
expressions, and an `INSERT ... SELECT` statement's query without volatile
output is pulled up into it the same way. Remaining: an action that also
reads other relations (its own `FROM`, an `UPDATE` or `DELETE` action, or
several `VALUES` rows), and an `INSERT` statement whose query has volatile
output, read a derived relation evaluated for every row first (one
`VALUES` row's expressions run before an action expression written ahead
of the reference), where
PostgreSQL's subquery scan streams it; an `INSERT ... SELECT` whose
condition and output both call routines evaluates every row's condition
before the outputs. A policy that a write's `UPDATE` (or `DELETE`)
and `SELECT` commands share runs once per row, as PostgreSQL's
`list_append_unique` keeps it; each command's restrictive policies run
before the OR of its permissive ones, which stops at the first that passes.
A row no partition accepts reports its partition key
(`Partition key of the failing row contains (id) = (21).`). That detail,
and the keys of unique, exclusion and foreign key violations, show only to
a user with `SELECT` on the table or on every key column, and never while
row security applies to the user, as PostgreSQL decides; otherwise a unique
violation has no detail and the others say `Key conflicts with existing
key.`, `Key is not present in table "p".` or `Key is still referenced from
table "c".`. A NOT NULL, CHECK or view check option violation's `Failing row
contains (...)` follows `ExecBuildSlotValueDescription`: a user without
`SELECT` on the table sees only the columns they may read or the statement
wrote, named (`(a, c) = (1, 3)`), no detail when there are none, and none
while row security applies. The written columns are an `INSERT`'s (or
`COPY`'s) target columns, or without a column list the leading columns its
source fills (a query's `*` counting each column it expands to), an
`UPDATE`'s `SET` columns, those of every `MERGE` action and, under `ON
CONFLICT DO UPDATE`, both. A view written through its `INSTEAD OF` trigger is
described by the view's privileges and the columns the statement wrote, and a
subscription's applied row by the applying role's privileges with no written
columns, as PostgreSQL's apply worker writes. Revoking a table privilege revokes it on
each column too.
A
query reading one plain table, and an `UPDATE`'s or `DELETE`'s scan of its
target, chooses its sequential, index, index-only or bitmap heap scan by
PostgreSQL's costs (`cost_index` with `btcostestimate`, its boundary
conditions and skip arrays, `cost_bitmap_heap_scan` over the bitmap
`choose_bitmap_and` and `generate_bitmap_or_paths` build, `cost_seqscan`,
`add_path` with its pathkeys, `cost_sort` and `cost_incremental_sort` under
`ORDER BY` and `create_limit_path` under `LIMIT`) from the statistics ANALYZE
gathers, and returns or changes its rows in that scan's order: an index scan
by key (backward for the reverse order), a bitmap heap scan and a sequential
scan by heap position (`differential_exact/691`, `692`, `720`-`723`): rows
saved to object storage first, then the resident versions written since,
also while a heap compaction is moving rows between memtable buffers.
Constraint validation, index builds, `VACUUM` and `ANALYZE` read rows in the
same order. The
model reads B-tree indexes of several key columns and pattern classes,
partial indexes whose predicate the conditions imply (`predicate_implied_by`
for comparisons with constants, NULL tests, `IN` lists and their AND and
OR), comparisons with constants, `IN` lists, `op ANY (array)`, NULL tests,
boolean columns tested by themselves and their `OR`, and an `ORDER BY` of
several columns. Expression keys are variables of their own, as
`examine_variable` matches them: ANALYZE gathers each index expression's
statistics (`compute_index_stats`, over the rows a partial index keeps),
`pg_stats` shows them under the index's name, and a condition or `ORDER BY`
on the expression reads them and uses the index, its scan returning rows in
the expression's order (`differential_exact/730`). A `LIKE` with a fixed
ASCII prefix on a `text` or `varchar` column gives the prefix's range (or
equality) as lossy index conditions under `C` or a pattern class, with
`patternsel`'s selectivity, and a `varchar` column cast to `text` is the
column (`731`). The scan's cost and width include its target list
(`apply_scanjoin_target_to_paths`): each expression's type width and
evaluation cost, and sort keys the select list lacks.
A partitioned table or inheritance tree read alone, or written by an
`UPDATE` or `DELETE` joining nothing, is planned member by member
(`set_append_rel_pathlist`, `differential_exact/750`, `751`): each member
from its own statistics and indexes, then an `Append` of their cheapest
paths (`cost_append`, its startup path under a `LIMIT`), an ordered
`Append` of range or non-interleaved list partitions whose paths return
the query's order (`partitions_are_ordered`, reversed for a descending
order), or a `Merge Append` (`cost_merge_append`) sorting members whose
paths lack it; a partitioned table's members compute a select list they
do not return, an inheritance tree's `Result` above the `Append` does, and
the `Append`'s width weighs each member's by its rows. Execution follows
the plan: each member's rows in its path's order (sorted when the plan
sorts it), members in the `Append`'s order or merged through
`ExecMergeAppend`'s binary heap, members run-time pruning removes skipped,
and `EXPLAIN ANALYZE` counts each member's scan, sort and bitmap, a
`Merge Append` under a `LIMIT` counting the row each other member holds.
A write's resumed attempt describes its first attempt's member paths
again. ANALYZE of a partitioned table or inheritance parent records each
column's statistics over the table and its descendants (`pg_stats`
`inherited`), from a sample of their rows, which estimates the groups an
incremental sort above the `Append` reads; a partitioned table's
`reltuples` counts its partitions' rows and its `relpages` is -1. A
foreign member's rows are not sampled. A write's scan returns its new
values and row identity (`ctid`, and `tableoid` for a tree's member),
whose width and evaluation its costs include, and the write costs what
its input does. A prepared statement's generic plan chooses paths without
its arguments' values, at execution too.
`EXPLAIN ANALYZE`
counts what the chosen scan reads: rows its index conditions (and a partial
index's predicate) reach, `Rows Removed by Filter`, each bitmap index scan
of a `BitmapAnd` or `BitmapOr`, a bitmap heap scan's `Heap Blocks` and an
index-only scan's `Heap Fetches`, and each index scan's `Index Searches`. A
table without index paths for the query keeps its sequential scan whatever
its conditions. Pattern conditions follow `like_support.c` under `C`
(`differential_exact/752`): `LIKE` (its `ESCAPE` folded through
`like_escape`, as `EXPLAIN` prints it), `ILIKE` (its prefix stopping at a
letter), regular expressions (`~`, `~*`, their prefix read from the
anchored characters `pg_regprefix` finds, exact before a final `$`) and
`starts_with` (a prefix through its support function) bound an index by
their prefix, a non-ASCII one too (`make_greater_string`'s UTF-8
increment), with `patternsel`'s selectivity. A condition on an expression
no index holds takes PostgreSQL's defaults (`differential_exact/753`): an
equality `1/DEFAULT_NUM_DISTINCT` (a boolean's 1/2), an inequality
`DEFAULT_INEQ_SEL`, two bounds of one expression `DEFAULT_RANGE_INEQ_SEL`,
a NULL test `DEFAULT_UNK_SEL`, a boolean function a third, and a
comparison with a boolean constant is its other side
(`simplify_boolean_equality`). Two expressions of the relation compared take
`DEFAULT_EQ_SEL`, its complement or `DEFAULT_INEQ_SEL`; a top-level `X = X`
is `X IS NOT NULL` (`process_equivalence`), and `NOT` of a comparison,
pattern match, `IN` list, scalar null test, `AND` or `OR` folds to its
negation (`negate_clause`, `differential_exact/834`); a row's null test keeps
its `NOT`. An operator symbol no operator takes fails at analysis. A cast of a column to its own type is no
node, printed and costed as the column, and a binary-coercible cast costs
nothing; a `varchar` or `text` relabeling is the column for its index. A
dummy relation's `Result` is as wide as the select list's expressions. An
`UPDATE` or `DELETE` whose costs choose a sequential scan changes its rows
in heap order and reads no index; a resumed attempt reads the same order
under the statement's snapshot. A btree's distinct keys are counted as
`estimate_num_groups` clamps a product of columns' distinct values.
A call of a SQL function whose body is one plain expression is replaced
by that expression before planning and execution (`inline_function`,
`differential_exact/800`): its arguments, converted to the parameters'
types, take the parameters' places, under PostgreSQL's checks (a SQL,
non-`SECURITY DEFINER` function without `SET` clauses returning no
`record`, a body no more volatile than the function, strict when the
function is, using a strict function's every parameter, no argument used
twice that is volatile, a subquery or costs over ten operators, and no
recursion), and the body then folds in the call's place, its errors
`during inlining`. A column keeps the name the call gave it. An index key
over such a call is read as its body, so a condition on the call or on
the body uses the index. A set-returning SQL function in `FROM` that is
neither strict nor volatile becomes a subquery of its body
(`inline_set_returning_function`), which the planner pulls up, and
`EXPLAIN` preprocesses each subquery in `FROM` (views expanded, functions
inlined, constants folded) before pulling it up. A `Function Scan` costs
its call once and each row (`cost_functionscan`), its rows the function's
`prorows` or, for `generate_series` over integers and `unnest`, what their
support functions compute of constant arguments.
ANALYZE builds each statistics object's kinds over its sample
(`differential_exact/801`): every row of a table of up to `300 *` its
target rows (the object's, else its columns' largest, else
`default_statistics_target`), else a reservoir sample of that many, as
`statext_ndistinct_build` (each combination of two or more keys, the
Haas-Stokes estimate), `statext_dependencies_build` (each degree, those
of zero left out) and `statext_mcv_build` (groups sorted by count with
PostgreSQL's quicksort, kept while above `get_mincount_for_mcv_list`,
with base frequencies) do, rows with a value wider than 1024 bytes left
out of the MCV list and the dependencies; a table without rows keeps its
earlier statistics. An MCV list holds at most 100 items and each item's
values at most 128 bytes of text; more fail with `54000`. pg_stats_ext
and pg_statistic_ext_data show them in PostgreSQL's key order (columns
by attribute number, then expressions). The single-table planner applies
them as `statext_clauselist_selectivity` does: conditions on two or more
keys from the MCV list (`mcv_clauselist_selectivity` combined with the
simple estimate), then equality conditions from the strongest
dependencies (`dependencies_clauselist_selectivity`), and
`estimate_num_groups` counts the groups of a statistics object's keys
from its ndistinct (`estimate_multivariate_ndistinct`). Remaining: the
estimates of joins, of plans outside the single-table planner (the
general `GROUP BY` costing among them), and of an index condition PostgreSQL derives from an `OR` ignore the
statistics objects without `0A000`, and pg_stats_ext_exprs shows an
expression key's null fraction, distinct values and width, not its MCV
list or histogram.
An index key or predicate may call a PL/pgSQL function
(`differential_exact/755`): a utility statement that evaluates outside a
query (`CREATE INDEX`, `REINDEX`, `CLUSTER`, `VACUUM`, `ANALYZE`,
`ALTER TABLE`, `CREATE TABLE AS`, materialized view population) runs such a
call in the engine and runs again replaying the results in their order, and
a write prepares the keys of the rows it stored as it ends, one row at a
time. An index key's calls are kept by their site (the row, the index and
the key's position), so the build's layers (tuple sizes, the sorted build,
the uniqueness caches) read one result and a function's notice is raised
once per row and index (`differential_exact/833`); a new index forms only
its own keys, and `REINDEX TABLE` builds its indexes one at a time in OID
order. An attempt collects every call it lacks, reads NULL in their place
and runs them together before restarting, so a build restarts as often as
its keys nest calls, not once per row. Remaining: a key whose calls nest
restarts once per row, to keep their order; `REINDEX INDEX`, a write and a
table rewrite (`ALTER TABLE ... TYPE`, `CLUSTER`) rebuild the table's value
caches, which evaluates the other indexes' keys again and repeats their
notices; a partial unique index's predicate and key calls interleave in
another order. An index whose expression no longer resolves after
`ALTER COLUMN ... TYPE` fails with `function rw(bigint) does not exist` and
its hint, without a cursor (`differential_exact/863`). Remaining, which `EXPLAIN` rejects with `0A000` naming the
shape whenever PostgreSQL could choose an index path: other index access
methods, expression indexes whose keys still call a user-defined routine or read
a user-defined type after inlining, a set-returning SQL function in `FROM`
PostgreSQL inlines with a `LATERAL` argument (the inlined subquery would join
parameterized, a plan of the join planner), and a window `PARTITION BY` over
an expression, whose `WindowAgg` target list prints names where PostgreSQL
prints expressions. A call of a SQL function PostgreSQL inlines packs a
variadic input's elements into an array (zero elements do not exist), resolves
a polymorphic input or result from its arguments, and inlines in a `MERGE` or
an `INSERT`'s `RETURNING`, with a user-type literal and, in a subquery,
reading the enclosing query's columns: the folding preprocesses each
sublink with the enclosing levels' columns in reach, so an `EXISTS` over such
a call joins as PostgreSQL's plan does (`differential_exact/890`, `891`,
`892`, `893`). A set-returning function in `FROM` inlines with a window
body, a column coerced to the declared type (not printed at the top of a
target entry), and polymorphic or variadic arguments (`894`); its rows come
from `generate_series` and `unnest` support functions over stable arguments
too (`895`). Remaining: the `VERBOSE` target list of a subplan reading
the enclosing query's columns (`Output: ?column?` where PostgreSQL prints
`(t.a + 1)`), a regular
expression with embedded options or a director (`(?i)`, `***=`),
conditions on an expression key of a statistics object (an `OR` through
an MCV list is estimated with `is_or`, `differential_exact/836`), conditions a
statistics object on a column past the 63rd or with an MCV list of a type
other than the integer, floating, numeric, character, `name`, `"char"`,
`oid`, boolean, date, time, timestamp, interval, `bytea`, `money`, network,
bit string, `pg_lsn` and UUID types estimates, a sub-partitioned table
whose partitions have index paths, an `ORDER BY` of an index expression
of a tree, a tree with a foreign member whose other members have index
paths, and, under `EXPLAIN ANALYZE`, `Index Searches` of an array or
skip-array scan over several leaf pages. A `SELECT` of plain columns from
two to eight plain, analyzed local tables without row security, joined by
inner, cross, left, right and full joins on equalities of same-typed
columns (integer, `text`, `numeric`, `date`, `bool`), comparisons (`<`,
`<=`, `>`, `>=`) of two such columns and conditions of one relation, is
prepared as PostgreSQL 18 prepares it (`reduce_outer_joins`, which also
makes a left join whose `WHERE` tests a compared column `IS NULL` an anti
join, `deconstruct_jointree` with `make_outerjoininfo` and commuting left
joins' cloned conditions, the distribution of each condition to the scan,
join or outer join that evaluates it, equivalence classes and
`reconsider_outer_join_clauses`), and is planned by PostgreSQL 18's join search
(`standard_join_search`, `add_paths_to_joinrel` with its merge, nested-loop
and hash paths, the join costs of `costsize.c`, `eqjoinsel` over the
columns' most common values, equivalence classes and `add_path`), and
`EXPLAIN` prints, and execution runs, the chosen plan. Each relation's
btree indexes give it index and bitmap paths (`create_index_paths`), the
ascending ones useful for merging keeping their order for merge joins,
and parameterized index and bitmap scans of the join clauses its key
columns take (`consider_index_join_clauses`), costed per outer row
(`get_loop_count`, `cost_index` and `genericcostestimate` with the loop
count) and run inside nested loops reading the outer row's values; a
unique inner relation (`innerrel_is_unique`) changes the nested-loop,
merge and hash costs and ends each outer row's scan at its first match,
and `Memoize` caches a parameterized scan's rows by its key
(`cost_memoize_rescan`), counting hits, misses and memory as
`ExecMemoize` does. A scan parameterized by relations on both sides of a
join gives a parameterized nested loop (`allow_star_schema_join`, with
`get_joinrel_parampathinfo`'s moved clauses and rows), as a link table's
two-column key does; a foreign key whose columns the join equates sets
its size (`get_foreign_key_join_selectivity`); and a merge join runs as
`ExecMergeJoin`'s states, marking and restoring its inner input, so
`EXPLAIN ANALYZE` counts what it fetched from each input
(`differential_exact/844`). A join column equated with a constant
restricts every member of its class to it and joins nothing
(`generate_base_implied_equalities_const`, `differential_exact/845`); a
class equating two different constants in the query's own join domain
empties the joined relation, which runs as a `Result` whose `One-Time
Filter` is `false` under the query's sort, limit or aggregate
(`differential_exact/872`).
Outer joins limit the join order (`join_is_legal`,
`have_join_order_restriction`), a full join plans its sides as separate
join problems (`make_rel_from_joinlist`), and each join's left, right,
full or anti paths (`populate_joinrel_with_paths`) are sized by
`calc_joinrel_size_estimate` with the matching conditions apart from the
filters pushed down to the join, an anti join's by `eqjoinsel_semi`
through the clause's one cached non-inner estimate; `EXPLAIN` prints each
join's `Join Filter` and `Filter` cheapest first, and execution returns
unmatched rows with NULLs as `ExecNestLoop`, `ExecHashJoin` (a hash
join's unmatched inner rows in bucket order, its NULL keys hashed as
nothing) and `ExecMergeJoin` (its fill states) do, and an inner scan
parameterized by an outer join's clause runs under a nested loop
(`differential_exact/846`). A merge join's sort of a single column sorts
datums (`ExecInitSort`), so its `Memory` counts no tuples. The semi and
anti joins pulled-up `EXISTS`, `NOT EXISTS`, `IN` and `ANY` subqueries
become are planned the same way: a semi join whose inner relation a unique
index makes unique for its clauses becomes an inner join
(`reduce_unique_semijoins`); otherwise its inner relation can also be made
unique by its compared columns and inner-joined (`create_unique_path`,
`join_is_legal`'s unique-ified joins), as a `Unique` over a `Sort` or a
`HashAggregate` whose groups come back in the order `ExecAgg` walks its
`simplehash` table, and a hash join may run as a `Hash Right Semi Join`
returning each inner row at its first match (`differential_exact/847`). A
query whose subqueries pull up into such a join runs in the general
executor rather than a retained reader, and a scan's conditions name a
pulled-up subquery's relation's columns bare, as PostgreSQL prints them.
`USING` and `NATURAL` joins join by the equalities of their columns,
`SELECT *` reading a merged column's left column (a right join's right
one, a full join's both), and the top join's width is the query output's
(`apply_scanjoin_target_to_paths`), each listed column counted
(`differential_exact/848`). A parameterized index scan also filters by
every other join clause its parameterization moves into it
(`get_baserel_parampathinfo`): an outer join's equalities and column
comparisons no index applies, several clauses of one key column (each
applied, a class's equality once), and a clause reading a column an outer
join nulls, whose path then waits for that join; a `Memoize` over such a
scan keys by the moved clauses' outer columns, in binary mode when one is
not a hash equality (`differential_exact/849`). Nested loops, hash joins
and merge joins may themselves be parameterized where an outer join
allows it (`param_source_rels`), each evaluating the join clauses its
parameterization moves into it (`get_joinrel_parampathinfo`, its own
`joininfo` clauses included) and sized by its `ppi_rows`; a hash join
whose hashed input reads an outer row's values builds its table again at
each rescan (`differential_exact/850`). A left join to one relation no row
above reads, which a unique index's keys (each equated with the join's
left side or a constant) make match at most one row, is removed with its
conditions (`remove_useless_joins`); a column only the removed join read
stays in its relation's scan width, and a query left with one relation
scans it with the query's output width (`differential_exact/851`). Join
conditions may also be inequalities of two relations' columns
(`neqjoinsel`, for a semi or anti join the outer column's non-NULL
fraction) and `OR`s of comparisons, equalities and one relation's
conditions (`clauselist_selectivity_or`), evaluated as join filters
(`differential_exact/852`). A constant equated with a left or right
join's outer column equates its inner column too, inside the join's inner
side, the join clause giving way to a constant `true` that `EXPLAIN` does
not print (`reconsider_outer_join_clause`, `differential_exact/853`). A
single-column btree's NULL keys are laid out as a run of their own at the
end of the index, in tuples of a header and a null bitmap that share
posting lists when it deduplicates (`differential_exact/862`).
Hash joins build
their table on the first outer row and return each outer row's matches in
their bucket chain's order (re-chained as `ExecHashIncreaseNumBuckets` does
when the table outgrows its planned buckets), keep a one-batch table across
rescans, merge joins sort with PostgreSQL's quicksort, and nested loops
rescan their inner input or its `Materialize` store
(`differential_exact/842`). A hash join whose table outgrows `work_mem`
times `hash_mem_multiplier` runs in batches as PostgreSQL's does
(`ExecChooseHashTableSize`, `ExecHashIncreaseNumBatches`, the outer key's
skew buckets from `ExecHashBuildSkewHash` and `ExecHashJoinNewBatch`), its
rows assigned by `hashint2`/`hashint4`/`hashint8`/`hashtext`/`hashchar`
values, so rows, `Batches` and `Memory Usage` match
(`differential_exact/843`), a right or full join returning each batch's
unmatched inner rows once its outer rows are probed, batch 0's skew
buckets after its main buckets (`differential_exact/854`); a session sets
`hash_mem_multiplier`, which hash joins and hash aggregates read; keys of other types (`numeric`) keep the
planner out of such joins, and fail with `0A000` should their one-batch
table outgrow its memory while running. `EXPLAIN ANALYZE` counts these nodes, a hash's
`Buckets` and memory and a `Materialize` store included, except a merge
join over an input in index order that is not a plain index scan without
a filter (`0A000`). The query's `ORDER BY` of columns and a constant
`LIMIT` or `OFFSET` enter the search as PostgreSQL's `grouping_planner`
brings them in: the ordering's pathkeys (a column's class, or a new one
for a column no class holds as the output reads it, a constant's or an
already ordered class's key dropped) keep index and merge join orders
useful for it (`truncate_useless_pathkeys`, `right_merge_direction`,
`select_outer_pathkeys_for_merge`), a limit makes every relation weigh
startup cost (`consider_startup`), and the joined relation's paths are
kept presorted, sorted or incrementally sorted (`create_ordered_paths`)
under a `Limit` (`create_limit_path`), the cheapest of those running,
each sort key shown as its class's first member the target list holds
(`prepare_sort_from_pathkeys`, `differential_exact/870`). A one-table
query's `Sort Key` drops a key of a column an earlier key orders, as its
redundant pathkey. The select list's expressions cost each joined
row (`create_projection_path`), and a select list of plain aggregates
runs an `Aggregate` over the join's cheapest path, the join returning the
columns the aggregates read (`differential_exact/871`). Such a join's
`Sort` sorts as `tuplesort` does in memory (quicksort, or past twice a
limit's rows a bounded heap), and its `Incremental Sort` sorts batches of
its input's groups as `ExecIncrementalSort` does, so rows with equal keys
come back in PostgreSQL's order; `EXPLAIN ANALYZE` prints its full-sort
and presorted groups' counts, methods and average and peak memory
(`instrumentSortedGroup`), and under a limit the join below it stops once
the sort has read the rows it returns (`differential_exact/875`). A
semi join's inner relation made unique by an index derives its class
clauses from the outer relation to the inner one, as
`reduce_unique_semijoins` orients them (`differential_exact/873`). Merge
joins also run descending or with NULLs first, as a descending `ORDER BY`
asks of their class (`MJCompare`, `mergejoinscansel` with the direction,
`differential_exact/874`). A class equating two constants inside an outer
join's domain empties only that domain's relations, apart from lower left
joins' nullable sides (`get_join_domain_min_rels`): one relation becomes a
dummy `Result` whose columns print unqualified, several make their join
dummy (`populate_joinrel_with_paths`, `differential_exact/876`). A class
may hold two columns of one relation, which becomes a restriction
comparing them (`generate_base_implied_equalities_no_const`,
`differential_exact/877`). The join's rows are grouped as `add_paths_to_grouping_rel` plans it: a
`HashAggregate` over the cheapest path, returning its groups in the order
`ExecAgg` walks its `simplehash` table (each key hashed by its type's
default hash function: `hash_numeric`, `hashfloat8`, `timestamp_hash`,
`uuid_hash`, `hashbpchar`, `interval_hash`, `hashinet` and the others,
`differential_exact/881`), or a `GroupAggregate` over the join sorted
(fully or incrementally) by the grouping keys, which take the `ORDER BY`'s
leading keys' order and directions (`preprocess_groupclause`,
`group_keys_reorder_by_pathkeys`), sized by `estimate_num_groups`, with
the sort and limit over the groups (`differential_exact/878`). The
grouping keys and the select list may be expressions of the grouped
columns, `HAVING` filters the groups (costed by `cost_qual_eval`), `ORDER
BY` may name expressions and aggregates, a plain aggregate may carry a
`LIMIT`, and a sorted grouping feeds order-dependent aggregates
(`string_agg`, `array_agg`) its sorted rows (`differential_exact/879`).
`DISTINCT` over a join is a `Unique` over sorted rows or a
`HashAggregate` (`create_final_distinct_paths`, `differential_exact/880`).
Join keys of `int2`, `int4` and `int8`, or of `real` and `double
precision`, compare across types in one class
(`differential_exact/882`). An `OR` whose arms are `AND`s restricts each
relation by the `OR` of its own conditions
(`extract_restriction_or_clauses`), dividing the join clause's
selectivity by the restriction's, and a semi or anti join's `OR` of
one-relation conditions is estimated as an inner join's
(`differential_exact/883`); a
parenthesized join that `deconstruct_jointree` reads into the same
problem plans as one join search (`differential_exact/884`). Remaining
for that search: aggregates with their own `ORDER BY`
or `DISTINCT` (presorted aggregate input), join conditions of
expressions (`a.k + 1 = b.k`) and conditions over several relations
other than comparisons of columns (`COALESCE(a.id, 0) < b.id`), an `OR`
of join conditions that a semi or anti join evaluates
(`clauselist_selectivity_or` under `eqjoinsel_semi`), an `OR` join clause
that an indexed relation's parameterized scan evaluates (as a filter, or
as a `BitmapOr` of parameterized index scans), window functions'
`WindowAgg` costs (`cost_windowagg`, modeled for no query yet), a merge join over a partly sorted outer input (`Incremental
Sort`), a right-nested outer join (`a LEFT JOIN (b JOIN c) ON ...`) and
derived tables kept as subqueries (`SubqueryScan`), self-join removal
(`remove_useless_self_joins`), a constant equated with a full join's
compared column (`reconsider_full_join_clause`) or with an outer join's
column another outer join nulls, `UPDATE ... FROM` and `DELETE ...
USING` through the same search, more relations than
`join_collapse_limit` (several join problems) or `geqo_threshold` (GEQO,
not ported; such queries keep pos3ql's join order and refuse cost output
with `0A000`). A parameterized index scan's cost reads its index's pages:
an index built over loaded rows is sized as `_bt_buildadd` fills its
pages (a page closes when its free space and its last tuple's posting
list fall under the fill factor's reserve, that tuple moving on), but one
grown by inserts is the btree size estimate below. The layout is computed
from the key columns' statistics, so equal keys share posting lists only
when the keys repeat twice on average: a column whose keys mostly repeat
twice but average fewer (`g % 1501` over 3000 rows) is laid out one page
larger than PostgreSQL's real build, which needs the index's own key
counts. An index's `pg_class` row reports no pages or tuples where
PostgreSQL's reports its size. The local reference used to verify costs runs
on arm64, whose compiler fuses multiply-adds, so a cost there can differ
from x86-64's (and pos3ql's) in its last printed digit. Other queries'
joins are not costed as PostgreSQL costs them (`final_cost_hashjoin`,
`final_cost_nestloop`, `calc_joinrel_size_estimate`), so an `EXPLAIN`
showing costs of such a plan with a join fails with `0A000` naming the
join node, as does one of an
`INSERT` whose listed values or defaults need a computation or coercion
the model does not cost, of an `INSERT` into a view, and of a write whose
source the planner does not model; `COSTS OFF` prints the join tree pos3ql
runs (`differential_exact/810`). Its join method and order are pos3ql's:
two items joined by equalities of their columns or of expressions of each
(`s.a = (t.x + 1)`) hash the second (`differential_exact/811`), where
PostgreSQL's costs may hash the other side or merge; a function in `FROM`
whose arguments read no earlier item is scanned once and may be hashed,
as PostgreSQL's analysis marks only a function reading them lateral, and
`USING` and `NATURAL` joins hash on their synthesized condition. As
PostgreSQL's `ExecHashJoin` does, a hash join builds its table only once a
probe row passes its scan's conditions (unless it returns unmatched build
rows), so an empty outer side never evaluates the inner keys, and stops at
an empty table. A retained result stream or cursor reads a join hashed on
expressions as a nested loop, in that loop's order. Remaining
inexact without `0A000`: btree sizes, which the model computes from the column statistics
where PostgreSQL counts the pages its build (`nbtsort`) and later page
splits (`nbtsplitloc`) made, and the heap placement of rows a large update
moves (a pruned page's freed space, HOT chains), which also orders an
index's equal keys by their current heap position. Maintenance
operations (`CREATE INDEX` and the index builds of `ALTER TABLE`,
`REINDEX`, `ANALYZE`, materialized view population) evaluate under the
search path `pg_catalog, pg_temp`, as PostgreSQL 17's `RestrictSearchPath`
does, after their own names resolve (`differential_exact/754`). Grouping, `DISTINCT` and joins above the scan keep the
access model below. The heap and btree layouts PostgreSQL's costs read are
emulated: rows packed in physical order, an index laid out as CREATE INDEX
builds it, and a visibility map whose pages stay all-visible after VACUUM
until an update or delete (`relallvisible` then counts none) or an insert
(pages from the first inserted row's on). Rows without such a chosen scan
follow a model of PostgreSQL's access path rather than its cost-based plan:
a query or data change on one table whose condition constrains an index's
leading column takes rows in their first-insertion order, as an index or
bitmap scan reaches an updated row at its first position, and any other
takes them in physical order. A table with saved row files read without
index candidates keeps physical order.
An exclusion constraint element may be a column, a function call or a
parenthesized expression (`EXCLUDE USING gist (int4range(a, b) WITH &&)`).
Its elements' expressions together hold at most 256 bytes of stored text;
more fails with `54000`. An element may name any operator class of its
access method that accepts its type, built-in (`text_pattern_ops`) or a
user-defined btree class, a `COLLATE`, and, under btree, `DESC` and `NULLS
FIRST/LAST`; its operator may be user-defined when the class's family holds
it, and runs through the operator's function. Operator class options on an
element fail with `0A000`. `DROP COLUMN` of a column an
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
its place when the scan neither filters nor reshapes rows, also under a
plain aggregate, `differential_exact/841`), and each outer
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
pos3ql shows the hash join or nested loop it runs. An `UPDATE ... FROM` or
`DELETE ... USING` of one plain FROM table, with both relations analyzed,
local, without row security and without an index on a column its conditions
read, runs and prints the join PostgreSQL's costs pick
(`differential_exact/670`-`672`): it costs both orders of a hash join on the
column equalities between the relations (`initial_cost_hashjoin`,
`final_cost_hashjoin` with `estimate_hash_bucketsize`) and of a nested loop
rescanning its inner relation or a `Materialize` of it, from each scan's
pages, rows and filter, and keeps the cheapest as `add_path` does (fuzzily
equal paths by startup cost, then the first generated). A relation's rows
under its own conditions are those the conditions keep, which PostgreSQL's
statistics estimate; a relation whose rolled-back or deleted rows grew its
heap since `ANALYZE` is estimated by PostgreSQL from its current size, which
pos3ql does not model. Each relation's scan filters by its own conjuncts,
the hash join checks the equalities and the join filter the rest; the join
returns its rows in the plan's order and each target row changes on its
first join row, with that row's FROM row, so triggers and `RETURNING` see
PostgreSQL's order. The hash keys are any equalities between an
expression of the target and one of the FROM table (`t.id = s.k + 1`), and
a SQL-language routine's parameters, a PL/pgSQL routine's variables and
built-in function calls in the conditions are planned as PostgreSQL plans
them (`differential_exact/740`). A FROM table read under column aliases
plans the same join, its conditions and output lists printed with the
aliased names, as the range table entry's `eref` names them
(`differential_exact/798`). Other joined writes (several FROM items, a
derived table, relations without statistics, which PostgreSQL may
merge-join, a nested loop on an equality) keep the row-by-row executor,
whose EXPLAIN shows pos3ql's own join order and costs: they
hash their first FROM table when it is joined to the target by equality,
as a `SELECT` of the two tables would, and join later FROM items to it in
nested loops; PostgreSQL may instead hash the joined FROM items. A statement
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
over scans), in all four formats. A hash join's `Hash` node reports the table PostgreSQL
would build: buckets sized from the node's row estimate (at least 1024) and
doubled while its rows outnumber them, one batch, and `Memory Usage` from each
row's minimal tuple (as PostgreSQL stores the values the node returns) and the
bucket pointers. Remaining: PostgreSQL's batches for a build larger than
`work_mem` times 2 (`ExecChooseHashTableSize`, `ExecHashIncreaseNumBatches`,
which need each row's PostgreSQL hash value and size), the skew buckets it
builds then for an outer key with most-common-value statistics
(`ExecHashBuildSkewHash`), and values it stores compressed or out of line
(rows over about 2 kB) are not modelled; a value type without a PostgreSQL size here (arrays,
`jsonb`, ranges, records) reports pos3ql's own table bytes. A scan's
cost and row model covers comparisons of a column with a constant, with an
InitPlan's output (a constant of unknown value: `var_eq_non_const` and
`DEFAULT_INEQ_SEL`, the InitPlan's parameter usable as an index condition)
and with a stable expression estimated from its value at planning
(`estimate_expression_value`), whose evaluation an index scan charges once at
startup (`index_other_operands_eval_cost`) (`differential_exact/783`).
`min` and `max` of the columns of one plain table are explained as
PostgreSQL's `preprocess_minmax_aggregates` plans them: each an InitPlan
reading the first row of the index scan ordered by its column with nulls
excluded, when those scans cost less than the aggregate
(`differential_exact/791`). Execution runs those InitPlans, and a nested
query's `ORDER BY ... LIMIT` over a scan returning rows in its order stops
at the limit as PostgreSQL's `Limit` does, so `EXPLAIN ANALYZE` counts one
row per InitPlan and the rows a filtered scan passed over
(`differential_exact/795`). Remaining there: a stable expression the model cannot evaluate
(a user-defined function) leaves the crude estimate. A built-in function's
argument of another type than it declares is charged its cast function
(`length(name)` reads `text(name)`), unless the types are binary coercible,
and an operator between two integer types or between `real` and `double
precision` casts nothing, as PostgreSQL's cross-type operators do
(`differential_exact/793`). `real` against an integer or `numeric` operand
takes the `real`-`double precision` operator, so EXPLAIN and view
definitions print the other side as `double precision` (`'2'::double
precision`) and the result type is `double precision`
(`differential_exact/796`). Aggregation over the chosen scan of one
plain table is costed as PostgreSQL's `cost_agg` costs it
(`differential_exact/790`): each transition state's function and input
expressions per input row and each final function once per group
(`get_agg_clause_costs`, from PostgreSQL 18.6's `pg_aggregate`, states shared
as `find_compatible_trans` shares them), a plain `Aggregate`, and for a
`GROUP BY` of the table's columns a `GroupAggregate` over a `Sort` weighed
against a `HashAggregate` with its spill model (`hash_agg_entry_size`,
`hash_agg_set_limits`, `Planned Partitions`) and the `ORDER BY` sort above
either, the grouping taking the `ORDER BY`'s key order
(`preprocess_groupclause`) and the groups `estimate_num_groups`.
Under `EXPLAIN ANALYZE` a `GroupAggregate`'s `Sort` returns every input row
and the aggregate one row per group in the sort's key order, and the JSON
form names it an `Aggregate` of strategy `Sorted` (`differential_exact/794`).
Grouping sets are arranged as PostgreSQL's planner arranges them: the
sets chained into rollups (`extract_rollup_sets` with its bipartite
matching, `reorder_grouping_sets`), then every set hashed when the tables
fit in `hash_mem` (a `MixedAggregate` when an empty set needs the sorted
phase), the input sorted for the first rollup with the rollups that fit
hashed (`DiscreteKnapsack`) and the rest sorted again, or every rollup
sorted, kept by cost; each node lists its `Hash Key`, `Group Key` and
indented `Sort Key` lines and the JSON `Grouping Sets` groups, and over one
plain table its costs and rows are PostgreSQL's (`differential_exact/802`).
Outside that cost model (the shapes below) the rollups and keys are
arranged from pos3ql's own estimates, so a sorted arrangement PostgreSQL's
costs would pick may show as hashed. A `HAVING` condition over aggregates
(`=`, `<>` and inequalities against constants, combined with `AND` and
`OR`) adds its `cost_qual_eval` per group, an aggregate costing what a
column does, and keeps the groups its default selectivities keep
(`DEFAULT_EQ_SEL`, `DEFAULT_INEQ_SEL`); a constant `LIMIT` or `OFFSET`
scales the run cost of each grouped and ordered path as
`adjust_limit_rows_costs` does (which the scan model's limit now also
follows when both are present), bounds the sort of the groups and picks the
cheapest limited path (`differential_exact/821`). Remaining: aggregates
with `DISTINCT` or `ORDER BY` inputs, user-defined aggregates, a select
list computing expressions over aggregates, an `ORDER BY` of an aggregate,
grouping by expressions, by a column an index orders (presorted and
incremental paths) and over joins keep pos3ql's own estimate. JSON nests as
deeply as PostgreSQL 18.6's `json` and `jsonb` inputs do under the default
`max_stack_depth` (21,790 and 18,675 levels on the reference build), and
deeper documents, or a recursion that would exhaust the query thread's
fixed stack, fail with `54001` "stack depth limit exceeded"
(`differential_exact/747`); PostgreSQL's limit varies with each operation's
frames, which pos3ql does not model. A query without FROM drops sort, grouping
and distinct keys that are planner constants, as PostgreSQL does (a constant
`DISTINCT` becomes a `Limit`). Conditions print coerced numeric literals as
the constants the planner folds them into (`'2'::double precision`). Times are measured
per loop, to the first row and to the end; in pos3ql's push-based executor a
node's time also includes the work of the nodes reading its rows. A locking
clause plans a `LockRows` node below the `Limit`, which counts the rows it
locked; rows read in scan order are locked as the scan returns them, and the
scan stops once the limit has its rows, as the `Limit` pulls them. An analyzed sort prints PostgreSQL's `Sort Method` (`quicksort`, or
`top-N heapsort` when a limit directly above bounds it and it read more than
twice the bound, which pos3ql then also keeps to a heap of the bounded rows;
`external merge` when its rows spilled to disk) and the memory
`tuplesort_get_stats` reports: its `SortTuple` array (1024 slots, grown as
`grow_memtuples` grows it) and each row's minimal tuple, a maxaligned bump
chunk, an aset.c chunk when a limit may bound the sort, nothing for a datum
sort of one column; a bounded heap charges the rows `make_bounded_heap`
leaves charged, including the rows it replaced while heapifying
(`differential_exact/476`, `820`). A sort's rows hold its output columns,
the sort keys that are not among them and the `ctid`s a row lock reads; a
`GroupAggregate`'s input sort is sized only when its aggregates read no
column (pos3ql's grouping keys lack the aggregates' inputs), and a value of
a type without a PostgreSQL size here (`jsonb`, arrays, records) or a sort
past `work_mem` keeps pos3ql's own figure. A hash aggregate prints `Batches:
1  Memory Usage` as `hash_agg_update_metrics` measures it once its input
ends: the meta context (first block, the bucket array `simplehash` sizes
from the planned groups and grows at its 0.9 fill factor, the table's
expressions measured on PostgreSQL 18.6 for one to seven grouping columns,
and the expression context under it), the bump context of the entries and
the context of by-reference transition states (array initial values and the
`Int128AggState` of integer averages and sums are modelled). `EXPLAIN
ANALYZE` of a hash aggregate with another by-reference state (`numeric`
sums, text `min`/`max`, collecting aggregates) or more than seven grouping
columns fails with `0A000`; a table that would spill (`Batches` above 1) is
not modelled, and outside the cost model the bucket array is sized from
pos3ql's group estimate. A `CTE Scan` prints its query's tuplestore
(`tuplestore_get_stats`: 2048 row pointers, doubled when a row reaches the
last free slot, and each row's generation chunk), also when it never ran;
a `Recursive Union` the sum of its working and intermediate tables, each
cleared before it is refilled; a nested loop's `Materialize` the tuplestore
of the inner rows its scan read (`differential_exact/537`, `561`, `534`,
`536`, `820`). Each window clause plans its own `WindowAgg` in PostgreSQL's
evaluation order, named `w1`, `w2`, ... when unnamed and printing its
`Window:` definition (with the `ROWS UNBOUNDED PRECEDING` frame PostgreSQL
gives windows of frame-independent functions), over a `Sort` of its input
or an `Incremental Sort` when the rows already follow a prefix of its keys;
a query `ORDER BY` the last window's rows follow needs no sort. Under
`ANALYZE` a window prints its `Storage:` line, and the query's `Incremental
Sort` over a window prints its `Full-sort Groups` and `Pre-sorted Groups`
lines, batched as `nodeIncrementalSort.c` batches its input. Remaining: the
window's storage, the sorts under windows and the incremental sort's group
memory are pos3ql's own figures; PostgreSQL's need the window's
`tuplestore_trim` points (its read and mark pointers per frame) and each
batch's tuplesort reuse, which `differential_exact/472` and `564` still
mask. A
condition folded to false keeps the query's sort, limit, row locks,
aggregation and windows over a `Result` with a false `One-Time Filter`. A
subquery the planner does not merge runs as a subplan: an uncorrelated
scalar, `EXISTS` or `ARRAY` subquery as `InitPlan n` under the query's top
node (referenced as `(InitPlan n).col1`, and gating the scan from a `Result`
when a condition reads only such subplans; the top node, and a gating
`Result`'s scan, include its cost as `SS_charge_for_initplans` charges it,
`differential_exact/741`), a correlated one in the select
list or `WHERE` of a one-relation query as `SubPlan n` after the inputs of the
node evaluating it, numbered as PostgreSQL plans them, with the subplan's
relations named uniquely (`u_1`). Under `ANALYZE` each subplan's nodes
report their own figures, once per run. A sorted query whose select list
calls a volatile function, or under `LIMIT` a user routine or a correlated
subplan, plans the `Result` PostgreSQL adds above the sort to evaluate that
list (`make_sort_input_target`), which hosts the list's subplans; execution
still evaluates the list before sorting, so such a subplan runs, and a
routine is called, once per input row where PostgreSQL runs it only for the
rows the sort and limit return. A top-level query's `WHERE` conjunct
`EXISTS`, `NOT EXISTS`, `x IN` or `x op ANY` over a subquery that reads one
stored table with only a non-volatile condition (correlated for `EXISTS`;
`IN` and `ANY` may be correlated too, as in PostgreSQL 18) becomes a semi or
anti join, as PostgreSQL's `pull_up_sublinks` makes it, and runs as one;
an `IN` or `ANY` output may be computed and a row operand compares its
fields with the outputs (`(a, b) IN (SELECT x + 1, y ...)`), while an output
that groups, windows, returns a set or calls a volatile function keeps the
subplan, where PostgreSQL joins the unflattened subquery
(`differential_exact/811`):
two relations joined by column equalities hash the smaller side (`Hash Semi
Join`, `Hash Anti Join`, or the `Hash Right` forms when the query's
relation is smaller), others loop over a materialized copy (`Nested Loop
Semi Join`), whose scan reads only as far as a loop needed, or over the
inner scan itself when the outer side estimates one row. A joined column
equated to a query column the `WHERE` equates to a constant takes the
constant (`o.id = 30` and `i.oid = o.id` give `i.oid = 30`). Result streams
read these joins in the retained reader, which waits on cold reads at any
level. A compared subquery left in place prints as its comparison with the
subplan's column (`(ANY (id = (hashed SubPlan 1).col1))`, `(ALL (id <>
(SubPlan 1).col1))` over a `Materialize`), and `<> ALL` keeps its own form
rather than becoming `NOT IN`; that `Materialize` reports no `ANALYZE`
figures yet. Such a subquery stays a subplan when it reads
a relation named like one of the query's, when a second subquery reads the
same relation (PostgreSQL names it `t_1`), and PostgreSQL's alternatives of
hashing an expression key (`(big.id + 1) = b2.id`) or joining the unique
subquery rows (`Hash Join` over a `HashAggregate`) are not planned; pos3ql
probes an index per outer row where PostgreSQL's costs may prefer a hash
join or a bitmap scan. A relation never analyzed estimates its resident
rows where PostgreSQL assumes ten pages (`estimate_rel_size`), so
`EXPLAIN` costs and row figures differ there. `VERBOSE` prints each node's
target list as `show_plan_tlist` does: a scan or join computes the
select list with its resjunk sort, grouping and window keys and locked
rows' `ctid` (an aggregate's input the grouping columns and the columns
its other expressions read, a window's input its keys first), a scan
whose parent accepts any columns returns its physical columns
(`use_physical_tlist`), one under a `Sort`, `Hash` or `Materialize` only
the columns needed above it, an upper node prints an expression its input
computed in parentheses (`((x + 1))`), `Append` prints none, and columns
are qualified when the range table holds more than one relation. A
`Function Scan` prints its schema and `Function Call`, a joined relation's
filter qualifies its columns, and a one-table `UPDATE` or `DELETE` scan
returns the new values (coerced to each column) and `ctid`. One-table
writes print their `RETURNING` list (`old` and `new` qualified), a
one-row `INSERT`'s `Result` columns (an omitted or `DEFAULT` column as its
default) and a several-row one's `Values Scan` columns when its rows name
every column (`differential_exact/565`), and postgres_fdw
scans and writes build their lists in the same target-list module. A
`VERBOSE` plan holding a list the deparser does not cover fails with
`0A000` instead of printing a wrong `Output:`. A constant `OFFSET 0` or
`LIMIT NULL` adds no `Limit` (`limit_needed`), and a join input's
`Subquery Scan` without a filter of its own is removed
(`trivial_subqueryscan`), so the join's conditions name the subquery's own
relation; a relation a subquery reads again under a name the statement
already uses prints as `t_1`. A statement's `WITH` query that PostgreSQL
does not inline (`SS_process_ctes`: `MATERIALIZED`, read twice without
`NOT MATERIALIZED`, volatile, recursive or data-modifying) is its top node's
`CTE name` InitPlan, also when the statement is a set operation or a write
(whose columns then print qualified), numbered before the query's subqueries and read by
`CTE Scan` nodes without a `Materialize`; a derived table's or subquery's
own `WITH` list follows the same rule as that query's InitPlans, and a
subquery reading only its own `WITH` queries is uncorrelated; a recursive one is a `Recursive
Union` whose recursive term reads a `WorkTable Scan`, with `ANALYZE`
figures per iteration. A recursive `WITH` query runs an iteration at a
time as its readers pull rows, so one that only a `LIMIT` ends returns the
limited rows, and a subquery's `LIMIT` without `ORDER BY` stops its scan
once it has its rows. Still differing: an ordinary `WITH` query PostgreSQL
runs once still runs whole when first read, and a recursive one read by a
join, or by a recursive term that reads only part of an iteration, runs an
iteration at a time where PostgreSQL pulls single rows (so a `WITH` query
PostgreSQL never reads, or reads only partly under a `LIMIT`, runs whole
here, evaluating and erroring on rows PostgreSQL never computes, and its
`CTE Scan`'s storage counts every row); a `CTE Scan` in a
scalar subquery's `InitPlan` prints `never executed`; a
data-modifying `WITH` query's `INSERT ... SELECT` from a table prints no
`ANALYZE` figures (its write runs on a path that does not enter the query's
plan block), an `IN` over a subquery with its own `WITH` list pulls
up into a semi join once the `WITH` queries it inlines are substituted, but
one whose `WITH` query runs once (`MATERIALIZED`, or read twice by a set
operation) stays a subplan where PostgreSQL joins its `CTE Scan` or set
operation, a join condition
over a derived table whose `WITH` query PostgreSQL reads directly names the
derived table (`s.a` for `c.a`), `EXPLAIN` plans a `LATERAL` item and a
subquery's derived table with the columns to their left and of the
enclosing queries in scope, keeping no `Materialize` copy of a `LATERAL`
subquery; still differing there: a simple subquery that reads such
columns is not pulled up into its query (PostgreSQL merges it, so its
`Subquery Scan` or `Result` disappears and its conditions join the
query's), a single-relation query keeps a `Subquery Scan` PostgreSQL
removes as trivial when the query only aggregates or returns its columns
(`trivial_subqueryscan`), and a `LATERAL` subquery's own `Filter` on its
output is not printed. A
subplan's relation renamed after one of the
statement's (`r_1`, `e1_1`) prints its new name in its query block's
conditions and lists. When a derived table or `CTE Scan` joins a
stored relation of equal size, PostgreSQL's cost tie may pick the other
outer or hashed side. Not yet shown: row
subqueries as subplans, a correlated subquery of a join, the `ANALYZE`
group lines of a window's own incremental sort or of one under a `LIMIT`,
`VERBOSE` output lists (rejected with `0A000`) of a joined `UPDATE` or
`DELETE` other than one of a single FROM table read sequentially, of a
`MERGE` from a `VALUES` list with a `NOT MATCHED BY SOURCE` action (whose
join reads the list's row as a placeholder expression) or whose lists hold
a subquery, a several-row `INSERT` whose rows omit columns or
use `DEFAULT`, a defaulted identity, serial or generated column,
an `INSERT ... SELECT`'s `RETURNING` list and an `INSERT` into a view, a
`(record).*` item, a sort below a postponed select list (whose input PostgreSQL
computes without the postponed expressions), and an `EXISTS` subplan's
list (the output columns' names are printed there), per-node `Buffers`
and `WAL` (reported on the top node only), trigger time lines, an index-only
scan's `Heap Fetches` and a bitmap heap scan's `Heap Blocks`. `EXPLAIN
ANALYZE` of a joined `UPDATE` or `DELETE` counts the scans (each member's
under an `Append`), the `Hash` with its table's size or the `Materialize`
with its storage (`Storage: Memory  Maximum Storage`, as a `MERGE`'s
nested loop also reports it), the join with the rows its join filter
removed, and marks nodes that never ran (`differential_exact/670`,
`671`); one outside the plan above fails with `0A000`, as its executor
probes the FROM rows for each target row and checks every condition per
pair. A statement that runs again after an attempt stopped for a call counts each
node in the last attempt that ran it, so a data-modifying `WITH` query whose
write a later attempt keeps reports its `ModifyTable` (its `RETURNING` rows)
and scan figures. A `LIMIT` that stops `Unique`
early leaves its `Sort` without figures, as pos3ql deduplicates before
sorting. Where pos3ql's plan differs from
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
constant written inside a routine body; an index or exclusion key expression
positions an unknown column (`differential_exact/812`). Pattern, `IN`-list,
quantified and `IS DISTINCT FROM` operators, sort and grouping keys without
an operator, array constructors, `COLLATE` clauses, window names, repeated
utility options and domain constraints, `EXECUTE` and routine arguments
(unknown literals and row constructors converting to their parameter's type
at analysis) and privilege checks on a written schema are positioned too
(`differential_exact/814`). Remaining: an unknown table named in a
query's select list over a derived table whose `EXISTS` subquery reads the
same name (`SELECT sum(a.id) FROM (SELECT a.id FROM a WHERE EXISTS (SELECT
1 FROM b WHERE b.k = a.id)) s`) is positioned at the subquery's reference
rather than the select list's. The exact-error corpus compares every error's
cursor, psql's `LINE n:` excerpt and caret. A domain's base type modifier
follows the coercion context (`differential_exact/830`): an explicit cast
truncates, an assignment, implicit coercion or input rejects; an input
function's failure (`'{toolong}'::d[]`, a domain element's check) points at
the literal, and a constant's length coercion, which the planner folds,
fails without a cursor before any row is read, also when inlining a SQL
function. An untyped literal argument of a function in `FROM`, positional or
named, converts to its parameter's type at analysis, and a built-in's to the
selected overload's (`generate_series(1, '3')`, `differential_exact/831`).
Explicit `COLLATE` clauses that meet in one expression conflict while the
query level, its subqueries, an `UPDATE` or a `DELETE` is analysed
(`assign_collations`, `differential_exact/832`). An `INSERT ... SELECT`
source PostgreSQL pulls up (`is_simple_subquery`), an `ON CONFLICT DO UPDATE`
assignment and a `MERGE` assignment or value of a constant fail its length
coercion while the statement is planned, as an `UPDATE`'s does; an unknown
collation (`42704`, naming the schema-qualified name and the encoding, or
`3F000` for a missing schema) and a `COLLATE` on a type without collation
(`42804`) fail at the clause, with its cursor, whether or not a row reaches
it (`differential_exact/860`), also in an index key, an `ALTER COLUMN ...
TYPE`, an attribute of `ALTER TYPE` and a domain-typed value or column, which
the message names by the domain (`differential_exact/861`). A domain has a
collation (`pg_type.typcollation`, also of its array type and
`information_schema.domains`): the one `COLLATE` gives it, anywhere among its
constraints, else its parent's or its base type's, and a base type without
collation refuses one at the type name. Columns, attributes, `ALTER TYPE
... ADD ATTRIBUTE` and `ALTER COLUMN ... TYPE` targets of the domain take it
unless they write their own `COLLATE` (a column's `COLLATE` may also follow
its other constraints, but not a `DEFAULT` expression's own); `ALTER COLUMN
... TYPE` takes the new type's collation, not the old column's. A cast to a
domain with a non-default collation derives it, a `CREATE TABLE AS` column
keeps the collation its query derives, and `pg_collation_for` reports it. A
custom collation a domain uses is a `pg_depend` dependency of the domain and
its array: `DROP COLLATION` refuses it, and with `CASCADE` drops the domain
and the columns of its type (`differential_exact/864`). `DROP TYPE` drops a
domain as `DROP DOMAIN` does, freeing its slot (`differential_exact/865`).
`ALTER COLUMN ... TYPE` to a domain, enum or composite rewrites the
column through the type: a domain's checks and `NOT NULL` apply to each row, an
enum or composite needs `USING` unless the column has the type already (`42804`
with PostgreSQL's hint), and a default that cannot convert is refused
(`differential_exact/866`). Target and expression count errors of an `INSERT`, also with subscripted
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
postgres_fdw's connection cache, `postgres_fdw_get_connections`,
`postgres_fdw_disconnect(_all)`, `keep_connections`, invalidation on server and
user-mapping changes (committed by `COMMIT` or `COMMIT PREPARED`), cursor scans
(`DECLARE cN CURSOR FOR`, `FETCH fetch_size FROM cN`, `CLOSE cN`), its INSERT
text and `batch_size` INSERTs match PostgreSQL's loopback in
`tests/external/foreign-connection-cache.sql`. A query of one foreign table
ships its needed columns, the conditions `is_foreign_expr` accepts (built-in
immutable operators and functions over modeled built-in types, constants and
`IN` lists, ordered as `order_qual_clauses` orders them), the order the plan
reads its rows in (`ORDER BY` keys that are shippable expressions, the order
`DISTINCT` reads its rows in under a `Unique` node, and the one window
clause's partition and order keys), and `LIMIT`/`OFFSET` when nothing stays
local. A grouped query of one foreign table whose `GROUP BY` keys ship and
whose `WHERE` ships whole (a `HAVING` condition without aggregates moves to
`WHERE`) is one `Foreign Scan` of the grouped relation (`Relations: Aggregate
on (public.t)`, schema-qualified under `VERBOSE`), with `count`, `sum`,
`avg`, `min`, `max`, `bool_and`, `bool_or`, `every` and `string_agg`
(`DISTINCT`, `FILTER`, ordered arguments) shipped, `GROUP BY` by target
position, and its `LIMIT` shipped when the order is by an aggregate
(`differential_exact/507`, `509`). As `foreign_grouping_ok` does, a
select-list constant or star ships, any other expression that does not ship
is computed over the remote groups from the aggregates it reads (a volatile
one under `ORDER BY` after them, as `make_sort_input_target` postpones it), a
`HAVING` condition that does not ship filters the groups (`Filter:`), and an
order only this side computes, or by an aggregate over filtered groups
without a `LIMIT`, sorts them above the scan; read through a view or
subquery its columns print qualified (`differential_exact/580`, `582`). A
lone `min` or `max` of a shippable argument without `GROUP BY` reads the
first row of the remote scan ordered by it with its nulls excluded, in an
InitPlan under a `Result`, as `preprocess_minmax_aggregates` plans it; two or
more ship as one grouped scan (`differential_exact/583`). A foreign scan
under a window or a local aggregate returns the window's input or the whole
row, as a table scan does, and a `WINDOW` definition's keys are read whether
or not a window uses them (`differential_exact/581`).
A write's `RETURNING` list may read the remote row's `ctid`, which the remote
`UPDATE`, `DELETE` or `INSERT` returns after the columns it reads
(`deparseReturningList`; `differential_exact/584`). Direct `UPDATE`/`DELETE` (with statement triggers firing around the remote
statement; a row trigger for its event or a stored generated column keeps it
row by row) ships the columns its `RETURNING` list reads and projects that
list over the returned rows; a `RETURNING` that reads `old` or `new` runs row
by row (`create_modifytable_plan`); the row-by-row `WHERE ctid = $1` commands
(a stored generated column that depends on an updated one set to `DEFAULT`,
`RETURNING` all columns only for an `AFTER` row trigger) match; and `EXPLAIN
VERBOSE` prints the write node's `RETURNING` output (`differential_exact/506`),
with their plans, in `tests/external/foreign-pushdown.sql` and
`differential_exact/504`. A condition that compares with an uncorrelated
scalar subquery reads the InitPlan's value as `$n::type` (`printRemoteParam`),
and a prepared statement's parameter ships the same way: `EXPLAIN EXECUTE`
prints the argument as a constant in a custom plan and `$n::type` in a generic
one, an `IN` list of them as `ARRAY[...]`, and the scan sends the values when
it declares its cursor (`differential_exact/621`, `622`). A parameter `LIMIT`
stays local, and a condition reading only parameters or InitPlans gates the
scan from a `Result`, under which the scan returns its physical columns (also
for tables). An InitPlan of a `numeric`, character, time or interval type,
which may carry a type modifier pos3ql does not derive, stays local; a
correlated subquery's outer column does not ship as a parameter, and execution
sends a custom plan's arguments as `$n` where PostgreSQL's remote query
carries the values. Values sent to the server (`INSERT`, `UPDATE`, `COPY`
and parameters) print in postgres_fdw's transmission modes (ISO dates,
`extra_float_digits = 3`) whatever the session's settings. A connection that
fails reports `08001` `could not connect to server "name"` with libpq's
message as its detail (`differential_exact/620`). `COLLATE` prints as the
planner leaves it: a constant carries its collation (`'a'::text COLLATE
"C"`), any other operand is relabeled, which shows as a cast where implicit
coercions print (`((note)::text)`), and a sort key then names its collation
(`differential_exact/623`, also for tables). An `ORDER BY` key a `WHERE`
equality fixes to a constant orders nothing: a table needs no `Sort` and a
foreign scan ships neither the key nor, with no key left, the `LIMIT`
(`differential_exact/624`). Without a `LIMIT`, the remote order ships only
when postgres_fdw's sorted path costs no more than a local sort of the
unordered rows (`estimate_path_cost_size` without remote estimates, a
never-analyzed table of ten pages, `cost_sort`, `add_path`'s fuzz factor),
with the selectivities `selfuncs.c` assumes without statistics
(`differential_exact/625`); a condition outside that model (a pattern, a
function returning a boolean) keeps the remote order. Remaining differences:
`EXPLAIN` of a foreign table read with other relations (a join, or a
semi-join from `IN` or `EXISTS`), under column aliases, or in a derived table
below a join fails with `0A000`, since postgres_fdw's join pushdown
(`foreign_join_ok`) and its estimates for join inputs are not modeled;
execution reads such a table whole and joins it here, with PostgreSQL's rows.
A join over a `WITH` query that reads a foreign table still prints pos3ql's
join method where PostgreSQL's estimates may choose another. `EXPLAIN` with costs prints a foreign scan's
cost as zero. A grouped query with `DISTINCT` or a window over its groups
groups locally; a grouped query with a `LIMIT` whose remote order is by
grouping keys, or none, groups locally where PostgreSQL's costs may still
ship it over filtered groups (it ships `HAVING sum(id) > random() ORDER BY a
LIMIT 3` but not the same over `avg(n)`), and a local `GROUP BY` under a
`LIMIT` hashes where PostgreSQL groups the remotely sorted rows; a postponed
volatile expression over groups sorted here declines the remote grouping; a
view or subquery output the outer query does not read is computed where
PostgreSQL replaces it with a typed `NULL` (`remove_unused_subquery_outputs`,
also for tables), which also removes a window or aggregate only that output
reads and an unread set-operation branch column, and skips an error the
output would raise; `DISTINCT` with a `WHERE` clause and no `ORDER BY` is
hashed locally (as PostgreSQL's costs choose) with its scan's `Output`
listing only the read columns. A prepared statement takes custom plans for
its first five executions and its generic plan after them, as
`choose_custom_plan` does under `plan_cache_mode = auto`, without
PostgreSQL's comparison of the generic plan's cost with the custom plans'
average, which needs PostgreSQL's cost of every plan shape. A select list, condition or `ORDER BY` with a correlated subquery reads every
column, where PostgreSQL fetches the columns the subquery reads. A
query whose only FROM item is a foreign table, a scalar or `EXISTS` subquery
over one, and a foreign partition or inheritance child read its cursor one
`FETCH` batch at a time as the plan consumes rows, and close it as soon as
the plan stops (`LIMIT`, `EXISTS`); a window's second pass reads the fetched
rows again, and a statement restarted by routine calls reads the rows its
earlier attempts fetched and continues the same cursor. Remaining: a foreign
table joined with other FROM items is read whole before the join (a right
or full join's unmatched-row pass and hash builds read the whole input), and
a SQL cursor or a PL/pgSQL `FOR` loop over a query runs the query to
completion before the first row is returned, so both fetch every remote
batch where PostgreSQL fetches only what the reads need. The exact-error
fixtures compare every DETAIL, HINT, QUERY and CONTEXT line. The comparator
masks one value: the new table's OID in the `Key (conrelid, contypid,
conname)=(...)` DETAIL of a CREATE TABLE that repeats a constraint name,
which each server draws from its own counter. Every unique B-tree index build (CREATE UNIQUE INDEX, REINDEX, ALTER TABLE
ADD UNIQUE or PRIMARY KEY, a table rewrite's rebuild of its indexes, with
the constraints ALTER COLUMN TYPE recreates last, a partition's cloned keys,
ATTACH PARTITION) sorts its keys with
PostgreSQL's in-memory quicksort, reading rows in a sequential scan's
physical order, so it takes O(n log n) comparisons and names the first
equal pair PostgreSQL compares; ALTER TABLE builds a new key before its
other row checks, which report `contains null values` and `is violated by
some row` as PostgreSQL's table rewrite does. A build PostgreSQL would sort
in parallel or beyond its default `maintenance_work_mem` (64 MB) fails with
`0A000` when it finds duplicates; pos3ql estimates both conservatively from
its own row sizes, and a build whose keys outgrow the statement arena fails
with `54000`. Table-level NOT NULL constraints (`[CONSTRAINT n] NOT NULL
col [NO INHERIT]`) merge with column constraints in CREATE TABLE and adjust
an existing constraint in ALTER TABLE ADD, as PostgreSQL 18 does; CHECK
constraints record `conislocal` apart from their inheritance count. A
composite routine parameter accepts a `ROW(...)` argument, a PL/pgSQL
`record` variable holding an anonymous record rejects `(r).f1`, and a bound
cursor's query sees only the variables declared before the cursor.
A NOT VALID not-null constraint checks new rows only, until VALIDATE
CONSTRAINT or SET NOT NULL checks the stored ones, and DROP CONSTRAINT drops
a not-null constraint (`differential_exact/745`). A CHECK merges with a
same-named inherited one when their parsed predicates are equal, with
PostgreSQL's conflicts for a different predicate, `NO INHERIT`, NOT VALID or
NOT ENFORCED (`differential_exact/742`); a PL/pgSQL expression `(r).*` is a
target list, and a cursor may be declared in a nested block
(`differential_exact/743`, `744`). An exclusion constraint's build sorts the
rows by its equality elements and sweeps one range-overlap element in O(n
log n) comparisons and names the first conflicting row in heap order
(`differential_exact/746`); elements of other operators are compared only
among the rows its equality elements make equal (`differential_exact/792`)
and, with a range-overlap element, whose ranges overlap, swept in
lower-bound order with the rows whose ranges still reach the current one
(`differential_exact/797`). Without an equality or range-overlap element (a
box `&&`, a multirange) every pair is compared, and for a GiST constraint
PostgreSQL names the partner its index scan finds first, where pos3ql names
the first in heap order. Box `&&` is PostgreSQL's `box_ov`. Merged CHECK predicates
compare in their analysed form, as PostgreSQL's `equal()` compares cooked
trees: `a > 0::integer` and `a > 0` merge, `d > 0` and `d > 0::bigint` (another
operator) conflict; a predicate outside the deparser's typed coverage compares
as parsed. A declared column merged with an inherited one at another position
is reported as moved (`differential_exact/781`). PL/pgSQL row and record variables (a
trigger's `NEW` and `OLD` among them) are visible to analysis at every query
level of a statement: `(r).*`, `r.*` and `ROW(r.*)` expand a row-typed
variable, or a record variable holding a named composite value, inside derived
tables, `WITH` queries, set operations, grouped, `DISTINCT` and ordered levels;
a record variable holding an anonymous record or none fails with `42809`, and
`r.*` naming both a FROM item and a variable follows
`plpgsql.variable_conflict` (`differential_exact/780`).
COPY FROM into a foreign table queues its rows
until PostgreSQL's buffering window flushes (1000 rows, 64 KB of input, a
partition that cannot batch, the end of the data) and sends them in
`batch_size` remote INSERTs; the queue is reserved per connection at startup
and holds at most `foreign_send_bytes` (`54000` beyond). A batched INSERT keeps its queued rows in the
statement's row progress when it pauses for a PL/pgSQL call or a wait, so
they are sent when a batch fills or the statement ends, as PostgreSQL sends
them. libpq options beyond host, hostaddr, port, dbname, user,
password, connect_timeout, application_name and sslmode fail with `HV00D`.
`CREATE
EXTENSION postgres_fdw SCHEMA pg_catalog` fails because `pg_catalog` is a
read-only virtual schema. A foreign key that references a partitioned table
has one `pg_constraint` clone, with its two RI action triggers, per
referenced partition at any depth (`pc_a_fkey_1`, named against the namespace
when made and kept), added and removed as partitions are created, attached,
detached and dropped, and each referenced partition has `relhastriggers`
set, so psql's `\d` lists the keys referencing it; a referenced partition's violations, `DETACH` and
`DROP` report it as PostgreSQL does. A table can hold at most 256 such
clones (`54000`). `pg_get_partition_constraintdef(oid)` and psql's `\d+`
`Partition constraint:` line print the constraint PostgreSQL's
`generate_partition_qual` builds from a partition's and its ancestors'
bounds, and new bounds report PostgreSQL's overlap, default-conflict,
empty-range and hash-modulus errors with the partition and value its bound
search finds (`differential_exact/505`). A `HASH` partition key of an
integer, text, `varchar`, `bpchar`, `bool`, `"char"`, `oid`, float,
`numeric`, date, time, timestamp, `bytea` or `uuid` column routes rows with
that type's default hash function seeded as PostgreSQL seeds it, skipping
null keys, and `satisfies_hash_partition` answers as PostgreSQL's does
(`differential_exact/550`); any other key type fails with `0A000`. Partition
keys are plain columns: expression keys and a key `COLLATE` fail (`0A000`
and a syntax error). A scan of a partitioned table or inheritance parent
reads the members PostgreSQL's partition pruning (`partprune.c`: RANGE with
multi-column prefixes, LIST with NULL and default partitions, HASH,
booleans, `IN`, `<>`, `OR` and nested levels) and constraint exclusion
(self-contradicting conditions, a member's `CHECK` and `NOT NULL`
constraints) keep, and `EXPLAIN` prints the same members: an `Append` of
their scans named `pt_1`, `pt_2`, ..., one member scanned under the table's
own name, or a `Result` with a false `One-Time Filter` when none is left.
Constants and a custom plan's arguments prune at planning; a generic plan's
arguments and stable functions prune at executor startup (`Subplans
Removed`), and execution prunes with every value it knows. `UPDATE` and
`DELETE` list each member as a result relation (`Update on pt1 pt_1`),
`VERBOSE` prints each member's target list and `tableoid`, and `EXPLAIN
ANALYZE` counts each member's scan (`differential_exact/600`-`604`).
A select list or `UPDATE` computed over an inheritance tree runs in a
`Result` above its members' `Append`, which cannot project; `ONLY` writes
the parent alone, a `RETURNING` list prints under the first member's name
and a locked parent's members return their `tableoid`
(`differential_exact/610`). Outer joins null-extend the rows a partitioned
table or inheritance tree matches nowhere, and RIGHT and FULL joins return
each member's unmatched rows (`differential_exact/611`). A key compared
with an InitPlan's output or an enclosing query's column prunes during each
scan, so `EXPLAIN ANALYZE` marks members no scan reached `(never
executed)`; a write computes its uncorrelated scalar subqueries as
InitPlans, and a FROM-less InitPlan counts its row
(`differential_exact/612`). A partitioned table pruning empties at planning
is a `Result` returning no rows: hashed on an outer join's nullable side,
its columns unnamed when the plan scans another relation, and an inner or
semi join with it returns nothing (`differential_exact/613`). A foreign
partition or child ships the conditions it can to its server and shows no
filter, or keeps every one (`differential_exact/614`). An `UPDATE ... FROM` or `DELETE ... USING`
into such a table runs, and `EXPLAIN` prints, one join over the `Append`
of the members pruning keeps under its own conditions (FROM items'
columns prune nothing), joined as for a plain target, the `Append` hashed
or rescanned when PostgreSQL's costs pick it; the write lists each member,
`VERBOSE` prints each node's list and `EXPLAIN ANALYZE` counts each
member's scan (`differential_exact/630`, `671`). A `MERGE` joins its target, a
partitioned table's `Append` included, and its source as PostgreSQL plans
relations with statistics: a hash join building on the relation with fewer
planned rows (on a tie, the source, or the target when only unmatched
source rows reach an action), whose probe reads its rows in scan order and
each key's hashed rows later rows first, then the unmatched hashed rows,
those with a NULL key first; without an equality, a nested loop
rescanning the smaller relation from a `Materialize`. `ON` conditions
reading one side filter that side's scan unless the join keeps its
unmatched rows, `DO NOTHING` actions keep none, and a join keeping both
sides' rows without an equality between them fails as PostgreSQL's full
join does (`0A000`) (`differential_exact/631`). Each relation's scan checks
the `ON` conjuncts reading only that relation unless the join keeps its
unmatched rows, the hash join its key equalities and the join the rest,
as PostgreSQL distributes them. `VERBOSE` prints PostgreSQL's lists: the
join returns a plain target's `ctid`, the source columns each action reads
(its condition, then its values in target column order), with a `NOT
MATCHED BY SOURCE` action the source's whole row and the source columns
`ON` reads, the source's row mark, the source columns `RETURNING` reads,
then a partitioned target's `tableoid` and `ctid`; each scan returns its
part of that list, then the columns the join's conditions read
(`differential_exact/651`). `EXPLAIN ANALYZE` counts the scans (each
member's under an `Append`), the `Hash` with its table's size, the
`Materialize` and the join with the rows its filter removed, and reports
the actions' `Tuples:`; a hash join keeping no unmatched inner row stops
when either relation is empty, reading the outer one first when the inner
one costs anything to scan (`differential_exact/652`). An index on a column
`ON` does not read, or on the hashed relation's key columns alone, leaves
the hash join PostgreSQL picks, and a unique one marks the hashed relation
`Inner Unique` (`differential_exact/653`). PostgreSQL merges relations it
has no statistics for, sorted by key; such a `MERGE` runs as a nested loop
over its source rows in their order and its `EXPLAIN` fails with `0A000`,
as do one from a source other than a table or `VALUES` list, one with an
index on another column `ON` reads, on the outer relation's keys or a
partial or expression index (PostgreSQL may scan, join or merge by it as
its costs choose), and one with row security. A `MERGE` reads its target's
and a stored source's `tableoid` in conditions, values and `RETURNING`; a
`WHEN` condition reading another system column fails with `42P10`, and an
action reading a relation its kind cannot reach fails with `42P01` (or
`42703` for a bare column), during analysis as in PostgreSQL. `ctid`
fails with `0A000` there as in every statement (`differential_exact/650`).
Joined `UPDATE` and `DELETE` conditions read the target's `tableoid`.
Remaining, each
failing with `0A000`: a member with an index on a column the query's
conditions, keys or aggregates read (execution scans members sequentially,
while PostgreSQL scans each by its own index as costs choose, possibly
under a `Merge Append`); `EXPLAIN ANALYZE` of pruning by a subquery that
reads an enclosing query; a write whose
subquery is correlated, a set or row subquery, or in `RETURNING`; a full or
anti join with an emptied partitioned table, or an outer join with an
emptied inheritance tree; a foreign member that ships only some of its
conditions, and `VERBOSE` over foreign members; a whole-row reference to an
inheritance child with columns of its own. Joins and grouping over
partitioned tables use pos3ql's join and aggregation methods, which differ
from PostgreSQL's cost choices as they do for plain tables. A `WHERE`
conjunct that can raise an error (`id % 2 = 0`) runs above a join rather than
at its relation's scan, so `EXPLAIN ANALYZE` counts its rows in the join and
hashes them; and a hash join whose build is empty still reads its outer
relation, where PostgreSQL reads at most its first row; a derived table that
PostgreSQL pulls up into an outer join still prints under its own name.
ANALYZE gathers PostgreSQL's column statistics (`compute_scalar_stats`: most
common values, histogram bounds, correlation and `n_distinct`) from the same
sample PostgreSQL reads (`differential_exact/690`). `pg_stats` shows them as
an `anyarray` of the column's declared type, in text and binary form: enum,
domain and composite values, and an array column's values as an array of
arrays. Range, multirange and `tsvector` columns count every value distinct
and a type without equality leaves `n_distinct` unknown, as PostgreSQL's
`compute_range_stats`, `compute_tsvector_stats` and `compute_trivial_stats`
do. Remaining: the element statistics of arrays and `tsvector` columns, range
and multirange histograms, `compute_distinct_stats` for types without an
ordering, the widths of values PostgreSQL compresses, and the binary form
of a composite value inside a statistics array, which pos3ql sends as
text; a table larger than 300 rows per unit of the
statistics target is sampled at random, as PostgreSQL samples it, so its
figures differ from run to run. A btree index may use
`text_pattern_ops`, `varchar_pattern_ops` or `bpchar_pattern_ops`, which
order and compare strings byte by byte; the planner lets such a key serve
`=` and no collation-aware comparison, and a default key no `~<~`-family
operator, as PostgreSQL's operator families do. It does not yet turn a
`LIKE 'ab%'` or `~ '^ab'` prefix into PostgreSQL's `~>=~`/`~<~` (or, under
`C`, `>=`/`<`) index range with the pattern kept as a filter, nor scan a
pattern-class key for an explicit `~<~`-family range: those keys get their
own physical binding, whose range plans need PostgreSQL's prefix and
histogram selectivity to choose its bitmap scan. Outside the chosen scans
above, a B-tree range (`id >= 5 AND id < 9`, also under `enable_seqscan =
off`) is planned as a sequential scan until its value binding is durably
complete, and `=` on a B-tree key whose columns cover the query shows `Index
Scan` where PostgreSQL shows `Index Only Scan`. A routine parameter or result written with `%TYPE` may name a
table column but not a view column (`0A000`). When a block label and a row variable share the
first word, the variable wins only when it is declared at least as deep as
the labelled block's variable, where PostgreSQL compares block by block. A literal passed to a function's `regclass`
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

The macOS arm64 native test build is memory-bound. Measured with a
`RUSTC_WRAPPER` running `/usr/bin/time -l` on the `native-ci` profile (Apple
M-series, peak memory footprint), the library alone peaks at 5.57 GB and one
test executable holding every library test at 8.02 GB, more than a hosted
macOS arm64 runner's 7 GB. The library tests therefore form two suites, each
built by its own `native-test-build` job: `--features sql-fixture-suite` (the
SQL fixtures in `sql::tests`) peaks at 6.90 GB and `--features unit-suite`
(every other library test) at 7.11 GB. A build without either feature still
runs the complete set, and `tools/check-lib-test-suites.py` checks in CI that
the two suites together run every test exactly once. Remaining: each suite is
still within about 0.1 GB of the runner's memory, so the next large addition
of tests or library code needs a further split (for example the storage and
checkpoint units apart from the SQL units) before it swaps again. The test
modules use crate-private interfaces throughout (`run_with` drives `Engine`
with private transaction, cursor and responder state), and the crate keeps
that surface private (`unreachable_pub`), so moving them into integration
tests would publish the engine's internals.

Commit-time catalog history (`catalog_snapshot::capture_transaction`)
synthesizes each catalog a transaction's changes can affect twice, as
committed and as the transaction sees it, and records the rows that differ.
When every change edits one table (its definition, indexes, triggers,
policies or statistics), `reached_tables` selects that table's partition or
inheritance family and the tables one foreign key away in either direction
with their families, and `pg_class`, `pg_attribute`, `pg_attrdef`,
`pg_constraint`, `pg_depend`, `pg_index`, `pg_inherits` and `pg_trigger`
materialize only those tables' rows; any other change compares
every table. Lookups that resolve one table's row through another (a key's
parent, `relhastriggers`, a clone's partition index) still read the
complete catalog, through per-pass maps of each table's indexes and of
visible table names. Test builds repeat every selected capture over all
tables and fail on any difference. Profiling
`configured_transaction_capacity_covers_truncate_fanout_and_prepared_cold_recovery`
(300 tables chained by foreign keys, `native-ci` build) took it from 94.5 s
to 32 s. Remaining: `pg_type` (whose domain rows resolve their base type's I/O
routines through the rows it built, a table's row type among them),
`pg_shdepend`, `pg_description`, `pg_publication_rel`,
`pg_policy`, `pg_statistic_ext`, `pg_partitioned_table`, `pg_rewrite` and
`pg_sequence` still materialize every table's rows for a table change, and
changes to views, sequences, routines and types select every table.
