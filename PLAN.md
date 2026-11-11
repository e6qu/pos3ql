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
PL/pgSQL and SQL-language functions, and triggers. Writing statements, and any
statement after its first effect, still block that worker on a storage read. No measurement on separate machines has been published.

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
trigger frames; PL/pgSQL calls inside `PERFORM`, `SELECT ... INTO` and
declaration initializers; SQL-language function bodies, child calls and return
checks; SQL cursors executing from retained query state, including row locks
taken per `FETCH` and scrollable cursors that evaluate revisited rows again;
client output that never blocks the worker on a slow reader; large
single-table results resuming row by row across client, storage and lock
waits. The
[request path](docs/architecture.md#the-request-path) describes how saved state
is owned.

**Remaining:**

- **Functions:** callers and expression positions that have no owning query;
  PL/pgSQL return conversion that can wait. A SQL function that waits, called
  once per row of a multi-row statement, adds one extra call to
  `pg_stat_user_functions` (`SELECT f() FROM generate_series(1, 2)` counts 3).
  A top-level `SELECT` that the retained reader cannot hold (joins, sorting,
  grouping, subqueries) restarts once for each first call of a SQL-language
  routine, so its work grows with the square of those calls; single-table,
  `VALUES` and `generate_series` sources hand each call off on its row.
- **Write phases:** BEFORE/INSTEAD triggers, source scans, row locks, partitions,
  constraints, transition tables, rules, data-modifying common table
  expressions (CTEs), COPY, replication apply and deferred triggers. Keep the
  original command and rollback boundary. Replace the current explicit rejection
  of row progress in multirow updates with saved progress: a child statement must
  see earlier changed rows before the next row is written, and each operation
  keeps its own transition table and firing order.
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
unsupported case into an explicit error. Known gaps: renaming or moving a
built-in function fails with `0A000` where PostgreSQL changes the catalog
entry (every built-in name lookup must consult a renamed or moved row); a routine signature naming `cstring` or `"any"` cannot be written
(`GRANT ... ON FUNCTION int4in(cstring)` fails with `42883`) because those
pseudo-types are not pos3ql column types, and `pg_type` lists 182 of
PostgreSQL's 621 built-in types; a
listed built-in that is neither implemented nor an operator, cast or input
function fails with `0A000` when called; `DROP ROLE`'s `DETAIL` lists
owners by OID and privileges by grant order, where PostgreSQL lists
`pg_shdepend` rows in their physical order (insertion order in a fresh
catalog), which needs a durable stamp for each owner change;
`pg_attrdef` OIDs derive from
table slots rather than creation identities; built-in operators, types and other
catalog objects have no shipped `pg_description` text (built-in routines
do). An undeclared `PREPARE` parameter that PostgreSQL leaves undetermined
(`PREPARE x AS SELECT pg_typeof($1)`, `42P18`) is `text`, and protocol Parse
analyzes nothing, so a Parse that PostgreSQL rejects (`SELECT ($1).a` with an
unspecified type) fails at Describe or Bind instead. A SQL function whose earlier
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
query text. In a `BEGIN ATOMIC` body only a `SELECT` of expressions without `FROM` is
deparsed (`SELECT (x + 1);`); other statements are printed one per line as
written, where PostgreSQL prints its analyzed form (`INSERT INTO t (a, b)
VALUES (f.x, 'q'::text);`); rule actions (`DO INSERT ...`) are printed as written, and their pretty
form fails with `0A000`; policy expressions are not deparsed. Syntax and scanner errors in a client's
query report PostgreSQL's wording and cursor position (the `P` field that psql shows as `LINE 1:` and a caret), but
other errors do not yet: PostgreSQL also points at the unknown relation, column,
type, function or failed constant (`SELECT * FROM missing_table` reports
position 15), maps a syntax error inside a SQL function body to its place in the
`CREATE FUNCTION` text, and reports dynamic SQL failures (`EXECUTE 'SELEC 1'`)
with an internal query and position (`q`, `p`) plus a `CONTEXT` line, which
pos3ql does not send. An echoed token longer than the 96-byte parser message is
cut short. The scanner does not read Unicode-escape literals (`U&'\0041'`,
`U&"\0041"`, `UESCAPE`), so `SELECT U&'\0041'` fails as an unknown column
`u`, and an identifier longer than 63 bytes is rejected (`name ... is longer
than 63 bytes`) where PostgreSQL truncates it with a `NOTICE`. Assigning to an
array element or slice (`UPDATE t SET a[1] = 9`) fails with `0A000`. A slice
followed by an index (`a[1:1][1]`) indexes the slice where PostgreSQL treats the
whole list as a slice, and a subscripted scalar subquery is named `?column?`
where PostgreSQL names it after the subquery's column.
Checks the parser makes where PostgreSQL checks later
still fail with `42601` and `syntax error: <description>`: type modifiers
(`numeric(2000)` is `22023` `NUMERIC precision 2000 must be between 1 and
1000` in PostgreSQL), COPY targets and options, storage parameters, sequence,
role and large-object values, and SQL/JSON result types. An unsupported cast
reports `42804` `cannot cast integer to timestamp` where PostgreSQL reports
`42846` `cannot cast type integer to timestamp without time zone`.
PL/pgSQL `CALL` in a trigger function fails with `0A000`, and a procedure
called from PL/pgSQL that waits for storage or a lock runs again from its
start when its statement resumes (finished effects replay from the statement
journal) instead of resuming at a saved position inside the procedure. Relation row types: values and columns of a
view's row type, and dropping a relation through `DROP SCHEMA`, `DROP
MATERIALIZED VIEW` or `DROP FOREIGN TABLE ... CASCADE` while another table's
column uses its row type, fail with `0A000`; a `DROP TABLE ... CASCADE` that
reaches both columns and routines, or names several tables, reports one notice
per table or dependency kind where PostgreSQL reports one for the statement, and a routine's dependency on a row type is described as depending on the
table; a `DROP TYPE` or `DROP DOMAIN` whose type both routines and other
objects (columns, views, casts) use reports the routines separately from
those objects where PostgreSQL reports them together. Values and columns of a view's row type need the view's column types,
which pos3ql derives from the view's query on each use instead of storing them
as PostgreSQL's `pg_attribute` rows do. The
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
