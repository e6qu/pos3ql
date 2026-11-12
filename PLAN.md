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
  routines, subqueries, sorts in object-store mode or over non-volatile
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
  Reads after a write's last row (statement-level and `AFTER` triggers,
  foreign key checks queued as constraint events, transition tables, the
  next step's selection) read synchronously. The write completion already
  runs an event that waited again instead of skipping it; those reads still
  need to leave the statement's effect protection so they can wait.
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
  by row, so a call in the main statement sees all of its rows. A PL/pgSQL
  `WITH ... INSERT/UPDATE/DELETE` statement fails with `0A000`.
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
    (scalar, `IN`/`EXISTS`, grouped or bounded derived tables, scans of CTEs
    that run once, and derived tables whose FROM names collide with the outer
    query's, such as a `NOT MATERIALIZED` CTE read twice),
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
    Cursors over those shapes run at `DECLARE`: a scrollable cursor ordered by
    an expression that is not a source column returns saved values where
    PostgreSQL evaluates a volatile output list again, and a row-locking cursor
    over a join, or ordered by such an expression, locks its rows at `DECLARE`.
    Cursors over retained joins already read as they fetch; declared cursors
    do not yet use the retained sort.
  - Scrollable cursors whose revisited rows are evaluated again (volatile
    output or filter), ordered scrollable cursors and held cursors still save
    their history in `cursor_bytes`. A scrollable cursor moving back rereads
    its input from the first row; retained position checkpoints would bound
    that reread.
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
entry (every built-in name lookup must consult a renamed or moved row);
arrays of `cstring` (`ARRAY['a'::cstring]`, `'{a}'::cstring[]`) fail with
`0A000`, and `SELECT DISTINCT`, `ORDER BY`, `IN` and `GREATEST` over
`cstring` values succeed where PostgreSQL reports that `cstring` has no
equality or ordering operator; `pg_type` lists all 473
`pg_catalog` types but of `information_schema`'s 148 only its domains and their
arrays (the row types of its views, whose OIDs initdb assigns after importing
the platform's collations, need the pinned reference image); a
listed built-in that is neither implemented nor an operator, cast or input
function fails with `0A000` when called; `DROP ROLE`'s `DETAIL` lists
owners by OID and privileges by grant order, where PostgreSQL lists
`pg_shdepend` rows in their physical order (insertion order in a fresh
catalog), which needs a durable stamp for each owner change;
built-in operators, types and other
catalog objects have no shipped `pg_description` text (built-in routines
do). Parameter inference walks a statement in its own order rather than
PostgreSQL's left-to-right analysis, so a parameter typed by one use and passed
untyped to an `"any"` argument by another can be accepted where PostgreSQL
reports `42P18`, and a user-defined polymorphic argument left untyped becomes
`text` where PostgreSQL reports `42804`. A SQL function whose earlier
statements are not queries or data changes (`CREATE TEMP TABLE t(a int);
SELECT a FROM t`) has its result converted value by value at the call instead
of being type-checked like PostgreSQL's analysis just before the final
statement runs. Column defaults, CHECK constraints, routine parameter defaults
`RETURN` bodies, index predicates, trigger `WHEN` conditions and rule
conditions are printed in PostgreSQL's analyzed form (`'x'::text`,
`((k >= 1) AND (k <= 5))`), but only for literals, columns, parameters,
operators, `IN`, `LIKE`, `IS NULL`, `IS [NOT] DISTINCT FROM`, `IS [NOT]
TRUE/FALSE`, `CASE`, casts, SQL value functions and built-in function calls;
other expressions (subqueries, user-defined function calls, `$n` parameters,
array constructors and whole-row references) are still printed as stored, and
an index expression or view that passes a whole-row reference to a function
taking the table's row type (`CREATE INDEX ON t (f(t.*))`) fails with `42883`
because its validation types the reference as `record`. `pg_get_viewdef` uses PostgreSQL's
layout, including its wrap column, only for a view
over plain tables joined with `ON` conditions, without `DISTINCT`, `HAVING`,
comma or `USING` joins, subqueries or set operations; other views return their
query text. In a `BEGIN ATOMIC` body only a `SELECT` of expressions
without `FROM` is deparsed (`SELECT (x + 1);`); other statements are printed
one per line as written, where PostgreSQL prints its analyzed form (`INSERT
INTO t (a, b) VALUES (f.x, 'q'::text);`); rule actions (`DO INSERT ...`) are printed as written, and their pretty
form fails with `0A000`; policy expressions are not deparsed. Errors in a client's query report
PostgreSQL's cursor position (the `P` field that psql shows as `LINE 1:` and a
caret) for syntax errors and for unknown relations, columns, types, functions
and operators, failed constant input and non-boolean conditions, except a
folded or quoted identifier in a statement that waited for typed inputs and a
constant written inside a routine body. `INSERT INTO t (a[1], b) SELECT 1` reports PostgreSQL's
`INSERT has more target columns than expressions` without its position; other
target and expression count errors are positioned. PL/pgSQL statements, writes, `EXECUTE` and FOR queries run their `WITH`
queries, including data-modifying ones, in one new command as PostgreSQL
does; a cursor's query with data-modifying `WITH` queries runs at its first
read. Remaining: a PL/pgSQL write with `WITH` queries whose nested AFTER
trigger waits fails with `0A000` instead of resuming; and a data-modifying
`WITH` query runs whole before the main query reads it, so a trigger's or
called function's notices arrive in a different order than PostgreSQL's
interleaved execution. The index behind a primary key or unique
constraint cannot be named by `CLUSTER ON`, `CLUSTER ... USING` or `REPLICA
IDENTITY USING INDEX` (`index "t_pkey" does not exist`). A view's
`reloptions` lists its options in a fixed order (`security_invoker`,
`security_barrier`, `check_option`) where PostgreSQL keeps the order they were
set, and omits an explicit `security_invoker=false`. A `CHECK` constraint
marked `NO INHERIT` fails with `0A000`, and exclusion constraints support only
GiST over range columns (`EXCLUDE USING btree (a WITH =)` fails with `0A000`).
`pg_constraint` omits the system catalogs' own not-null constraints. A
misplaced column constraint attribute (`a int NOT NULL DEFERRABLE`) is
reported before a later syntax error in the same statement, where PostgreSQL
reports the syntax error first. A rewrite rule `INSERT` action whose `VALUES` rows write
`DEFAULT` in different columns runs as one insert per pattern, so `RETURNING`
from such an action fails with `0A000`. A cursor whose query
cannot stream runs it at `DECLARE` or `OPEN`, so a volatile function's side
effects happen before the first `FETCH` rather than at it. A derived table computes output
columns its query never reads, so `SELECT * FROM t WHERE EXISTS (SELECT 1
FROM (SELECT 10 / t.x) s)` fails with division by zero on a zero `x` where
PostgreSQL, which drops unread subquery outputs, returns the row. A
whole-row reference to a table (`SELECT t FROM t`) is typed `record`, where
PostgreSQL types it as the table's row type `t`. A row constructor casts to a table's row type
(`ROW(1, 'a')::t`) but not to a view's, and an array of a table's row type
(`ARRAY[ROW(1, 'a')::t]`) fails with `0A000`, where PostgreSQL accepts both.
A PL/pgSQL variable declared with a view's row type (`r v;` or
`r v%ROWTYPE`) fails with `0A000`; a table's works. In a trigger function, `OLD` and `NEW` are
assignable records with the table's row structure, as in PostgreSQL;
assigning a row of another structure to one fails at the assignment with
PostgreSQL's `returned row structure does not match` error, where PostgreSQL
accepts it and fails only if a BEFORE trigger returns it. A `CONSTANT`
assignment error has no cursor position: PL/pgSQL assignment targets do not
keep their place in the routine body.
Checks PostgreSQL makes in parse analysis or execution (type modifiers, COPY
options, storage parameters, sequence, database and large-object values,
SQL/JSON result types) are reported when the statement would run. In a
`SELECT`, an unknown relation, a modifier's unknown type and an output column
written before the failing modifier are reported first, as PostgreSQL's
analysis order finds them; in other statements the modifier error comes first.
A `RETURNING` list routed through a rule's `RETURNING` list reads the rule's
relation only: `UPDATE v ... FROM o RETURNING o.label`, `*` beside `FROM` or
`USING` sources, and subqueries in that list fail with `0A000` where
PostgreSQL evaluates them. A rule condition's `OLD` or `NEW` that the event
lacks (`ON INSERT ... WHERE OLD.n > 0`) is reported without its cursor
position, and a missing field of a PL/pgSQL record variable read in a `DO`
body (`RAISE NOTICE '%', r.nope`) carries a position where PostgreSQL names
the expression in `CONTEXT`. Selecting a field of a named composite cast to
`record` (`(ROW(1, 2)::t::record).id`) fails as an unknown field. Reading a
field of a NULL `record` parameter names the expression in `CONTEXT`, which
PostgreSQL omits because it raises the error while evaluating, not planning.
COPY to or from a server file or program,
`COPY ... FREEZE`, a COPY `ENCODING` other than UTF8 and `JSON_QUERY ...
RETURNING` a non-JSON, non-string type are rejected with `0A000`.
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
`pg_event_trigger_dropped_objects` is built by each statement rather than from
that walk, so a cascade lists its objects in the order the statement drops
them (`DROP TABLE t CASCADE` lists a view over a materialized view of `t`
before the materialized view) where PostgreSQL follows the walk; the walk's
objects would have to be described before they are dropped, into a fixed
per-statement buffer; `pg_depend` still lacks PostgreSQL's rows for types named only inside
expressions (a cast to a domain in a default or routine body) and the
column-level rows of views that read a sequence (`SELECT last_value FROM sq`
depends on `column last_value of sequence sq`). `pg_rewrite` lists no rules of built-in views (PostgreSQL has 145
`_RETURN` rules, at OID `reltype + 1`, and `pg_settings_u`/`pg_settings_n`),
and `ALTER VIEW` and `ALTER MATERIALIZED VIEW` accept only their own actions,
so `ALTER VIEW v DISABLE RULE r` fails with `42601` where PostgreSQL reports
`42809` `ALTER action DISABLE RULE cannot be performed on relation "v"`. A partition key that is an
expression (`PARTITION BY LIST ((k + v))`, `PARTITION BY RANGE (lower(a))`)
fails with `0A000`; PostgreSQL routes rows by the evaluated key and lists it
in `pg_partitioned_table.partexprs`. A table's or view's row type cannot
appear in a routine signature: `CREATE FUNCTION f(orders)` fails with `0A000`
where PostgreSQL accepts it. Under `READ COMMITTED`, a locked row's recheck runs a correlated `EXISTS
(subquery)` again, where PostgreSQL's semi-join recheck keeps the subquery row
it first matched (which row depends on its join plan), and a row another transaction moved to a different partition is
skipped where PostgreSQL fails with `40001` (`tuple to be locked was already
moved to another partition due to concurrent update`); storage does not yet
mark a version deleted by a partition move. A query with a `LATERAL` source
fails that recheck with `0A000` instead of running the source again. The
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
