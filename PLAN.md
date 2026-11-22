# Finish pos3ql

[Overview](README.md) · [Architecture](docs/architecture.md) ·
[What works today](docs/implemented-baseline.md) ·
[Limits](docs/postgresql-18-compatibility.md) · [Contributing](CONTRIBUTING.md)

This plan lists only unfinished work. Current behavior belongs in the
[guides](docs/README.md); finished plans are in the
[history index](docs/history/README.md). An item is deleted when its PR
merges, and a milestone when its "done when" is demonstrated.

## The finish line

pos3ql is finished when every condition below holds on the `main` branch and
is shown by a check anyone can rerun:

1. **Compatibility.** Every PostgreSQL 18 built-in function, operator, type
   and command is either verified against PostgreSQL 18.6 by an executed test
   or rejected with an explicit error (usually `0A000`). Every item in
   [Close application compatibility gaps](#close-application-compatibility-gaps)
   is closed or turned into such an error. The client suites (psql,
   pg_dump/pg_restore, psycopg2, psycopg 3, SQLAlchemy, Alembic, pgJDBC,
   Npgsql, node-postgres, pgx), the differential corpora and the vendored
   regression schedule pass on every native target.
2. **Durability.** A confirmed commit survives loss of the server's disk:
   the empty-cache recovery tests pass in both transaction modes (`mvcc` and
   `locking`).
3. **No stalls.** No statement holds a SQL worker while it waits for object
   storage, except where durable commit order requires it. A test with a
   delayed object-store GET shows another client making progress.
4. **Several cores.** Read, write and mixed workloads scale with the number of
   SQL workers, and every correctness gate passes with several workers.
5. **Fixed memory.** All memory, disk and concurrency limits are set before
   startup; no runtime heap growth occurs; running out gives a named error;
   shared pools follow demand.
6. **Failover.** A failed server is replaced from object storage by a tested,
   documented procedure that reroutes clients.
7. **Evidence.** Published, repeatable measurements against PostgreSQL 18
   support the stated sizes, transaction rates, bandwidth and failover times.

One active process publishes to each object-store prefix and keeps commits in
durable order. That order does not require running every read, upload or SQL
statement one at a time.

## Where we are

SQL, transactions in `mvcc` and `locking` modes, PostgreSQL clients, indexes,
logical replication, backup and recovery from object storage work today and
are compared with PostgreSQL 18 in CI. Object-store requests overlap and
commits share uploads. SQL runs on one CPU worker. Queries, cursors, routines,
triggers and writes pause during a storage wait and resume where they stopped;
joins planned as nested loops, hash joins and `Materialize` run as pull nodes
that do the same. DDL, `COPY`, PL/pgSQL cursor reads, commit processing,
replication apply and a few write steps still block the worker
([execution model](docs/architecture.md#execution-model)).

The project's core bet, that commits through object storage give useful
latency and cost compared with PostgreSQL, is untested: the newest comparison
(2026-09-23) ran MinIO and SeaweedFS on the benchmark host against an older
revision, and nothing has been measured on separate machines. Measuring that
is the next milestone.

## Milestones

Milestones ship in complete PRs, in the order of the table. Work already in
progress is finished, not abandoned. Compatibility work continues alongside,
behind the first three milestones.

| Order | Milestone | Status | Depends on |
|---|---|---|---|
| 1 | [Measure a first deployment baseline](#measure-a-first-deployment-baseline) | Next | — |
| 2 | [Isolate readers and bound cleanup](#isolate-readers-and-bound-cleanup) | In progress | — |
| 3 | [Pause and resume every query path](#pause-and-resume-every-query-path) | In progress (pull executor) | — |
| 4 | [Run SQL on several CPU workers](#run-sql-on-several-cpu-workers) | Not started | 2, 3 |
| 5 | [Close application compatibility gaps](#close-application-compatibility-gaps) | In progress; alongside, behind 1–3 | — |
| 6 | [Share fixed capacity by demand](#share-fixed-capacity-by-demand) | Partly done | — |
| 7 | [Qualify production deployment](#qualify-production-deployment) | Not started | 1–6 |
| — | [Adapt batching and layout with feedback control](#adapt-batching-and-layout-with-feedback-control) | Designed; non-blocking | 1, 6 |

The baseline's results may reorder milestones 2 to 7.

### Measure a first deployment baseline

**Goal:** a dated, reproducible measurement of commit latency, read latency
and object-storage cost per transaction against PostgreSQL 18, taken in
practice on separate machines. It tests the project's core bet.

**Step 1, on one machine.** Everything here runs on a developer host or
runner with Docker; it readies the harness so step 2 is a rerun, not a
debugging session.

1. [ ] `tools/run-performance.sh full` and
   `tools/run-performance-matrix.sh full` complete on the current revision
   with MinIO and SeaweedFS; fix at the root whatever fails. The last retained
   matrix is revision `90861f1b` (2026-09-23).
2. [ ] Count every object request pos3ql makes, by kind, with bytes:
   `/metrics` covers only immutable data-file reads through the cache today,
   not journal, commit-head, manifest, LIST or DELETE requests, so runs
   outside the instrumented fixture report request counts as unavailable.
   The report divides them by committed transactions.
3. [ ] Extend the report to the baseline's figures: typical (p50) and slow
   (p99) commit latency, point-read and scan latency with warm and empty
   caches, and object requests and bytes per transaction, beside both
   PostgreSQL 18 controls (host-available and resource-matched, see
   [performance](docs/performance.md)).
4. [ ] Run the matrix three times on one host and retain the raw results and
   a dated report in [benchmarks](benchmarks/README.md). This is a same-host
   rehearsal: it checks repeatability, not deployment speed.

**Step 2, on two fixed hosts with separate object storage.** The only
external dependency is an independently operated S3-compatible service.

5. [ ] Record the two fixed hosts and their roles, the network path and the
   object storage service, without credentials.
6. [ ] Run the suite's `external` profile repeatedly against both controls.
7. [ ] Retain revisions, raw results, latency distributions, CPU and RSS,
   request counts, bytes and cost inputs (request and storage prices with
   their source and date) in [benchmarks](benchmarks/README.md).
8. [ ] Review the milestone order against the results.

**Done when:** the step 2 baseline is published and the milestone order has
been reviewed against it.

### Isolate readers and bound cleanup

**Goal:** a reader finishes with the rows and definitions it selected while
schema changes, commits and maintenance continue; cleanup never removes
anything a reader or backup still needs. With pausing, this is a prerequisite
for several CPU workers.

**Remaining:**

- [ ] Catalog reads after listing an object refer to that same object
  (columns, indexes, comments, repeated dependency rows); unfinished edits stay
  private; an old listing and a current name lookup split as in PostgreSQL (an
  older report lists `orders` while `to_regclass('sales')` finds the renamed
  table).
- [ ] Old rows, indexes and saved files stay until readers and backups finish
  and are removed in bounded batches.
- [ ] Files of an unfinished checkpoint stay owned until its publication
  resolves; durable manifests never name unfinished uploads; switching output
  producers never cancels another job's files.
- [ ] Read counters survive savepoint rollback and release; a table reusing a
  slot never inherits a retired table's counters.
- [ ] Corrupt or stale restored state is rejected before writes are accepted;
  a lost commit reply resolves to all changes or none.

**Done when:** overlapping reads, schema edits, publication and maintenance
keep readers on their selected objects and values, new reads see whole
commits, and the rollback, savepoint, prepared-transaction, capacity and
empty-cache recovery tests pass.

### Pause and resume every query path

**Goal:** a statement waiting for object storage or another connection's lock
gives up the CPU worker and later resumes exactly where it stopped. The
[execution model](docs/architecture.md#execution-model) describes what pauses
today, and [query planning](docs/query-planning.md#pull-based-join-executor)
the pull executor.

**Finish the pull executor (in progress), in this order:**

1. [ ] Every plan the join search makes pulls but one scanning a `LATERAL`
   item: merge join, `Memoize` and `Unique` pull nodes run beside the
   others (`997`), and a hash join's keys always have a ported hash
   function, since the model plans joins only over such types. Still open:
   `EXPLAIN ANALYZE` counters live in the pull nodes, and a pulled-up
   subquery's join reads as a retained reader.
2. [ ] The general executor runs the same nodes. Delete the push-based
   `Runner` in `join_exec.rs`, the retained reader's `JoinState` levels and
   `MAX_RETAINED_SOURCES` (the eight-item limit on retained joins).
3. [ ] Sort on the `tuplesort` port, `HashAggregate` on `hashagg.rs` and
   `LATERAL` bodies run as subtrees of the same plan. `ORDER BY` without a
   limit over a join larger than the statement arena then sorts in runs over
   `plan_build_bytes` leases (`54000` today).
4. [x] A declared cursor (`DECLARE`, PL/pgSQL `OPEN`) plans for
   `cursor_tuple_fraction` of its rows (`CURSOR_OPT_FAST_PLAN`, `995`), and
   `EXPLAIN DECLARE` shows that plan, under a `Materialize` when a scrollable
   cursor's nodes cannot run backward. Still open: a set operation's members
   plan for every row, a grouping keeps only its cheapest paths under a tuple
   fraction (its sorted read of an index-ordered scan must first stop copying
   each whole row into the statement arena), and `EXPLAIN` of a scrollable
   cursor needing that `Materialize` fails with `0A000` under `ANALYZE` or
   when its plan has InitPlans.
5. [ ] Plan joins of never-analyzed tables by PostgreSQL's search (they keep
   pos3ql's join order today); this needs steps 1 to 3 so the result-stream
   and cursor tests keep streaming.

**Other paths that still block or restart:**

- [ ] A wait inside a write in a function called from a query no longer
  restarts the statement.
- [ ] PL/pgSQL cursor reads (`OPEN`, `FETCH`, cursor `FOR` loops) pause on
  storage reads (their lock waits already do).
- [ ] A top-level `SELECT` the retained reader cannot hold no longer restarts
  once per first call of a SQL-language routine (work grows with the square of
  those calls): retain `DISTINCT` over routine output, grouping sets,
  groupings whose keys or inputs call routines, correlated subqueries,
  object-store sorts, sorts over non-volatile routine output, and index-probed
  joins.
- [ ] A write whose scan runs a volatile built-in pauses instead of reading
  its rows synchronously.
- [ ] Reads after an `AFTER` event's first effect pause (a trigger body that
  writes, then reads cold data, blocks today).
- [ ] `AFTER` work (foreign key checks included) of a data-modifying `WITH`
  main statement, of rule steps inside a routine body, and of writes inside
  functions called from a restartable statement pauses: give them the step
  record of their enclosing write's attempt loop. A data-modifying `WITH`
  main statement's row selection pauses.
- [ ] A join reading a data-modifying `WITH` query pulls it as PostgreSQL's
  plan does (a nested loop under `LATERAL`, or a hash join that hashes the
  other input) instead of reading it whole at its first read.
- [ ] `INSTEAD OF` view rows, DDL, `COPY`, replication apply and deferred
  triggers pause on storage reads.
- [ ] Row progress for data-modifying `WITH` inside routine bodies, so a
  routine call after their first row change no longer fails with `0A000`
  (`SQL row expression requires retained write progress`).
- [ ] `INSERT ... SELECT` from a source calling user routines streams with
  per-row result storage released once the row is written (today it
  materializes, and a source larger than the work arena fails with `54000`).
  Calls in a materialized source's `WHERE`, join conditions, subqueries,
  aggregate arguments or set-operation branches run per row, so they see rows
  the statement inserts.
- [ ] Retained shapes beyond joins: correlated and grouped or bounded
  subqueries (rewinding a child tree per outer row instead of re-analysing);
  set operations other than `UNION ALL` (and `UNION ALL` under `ORDER BY`, in
  a declared cursor, or with volatile or routine-calling expressions);
  set-returning functions with non-constant arguments, user-defined ones and
  column definition lists (`jsonb_to_recordset(...) AS t(a int)`).
- [ ] Object-store sorts stream through a run reader per stream (today a
  large ordered `SELECT` there spools, and a paged portal over one buffers in
  `portal_result_bytes`). Declared cursors use the retained sort and read the
  retained shapes as they fetch instead of running the query at the first
  read (a scrollable cursor ordered by a non-column expression re-evaluates
  volatile output; a row-locking cursor over a join locks per fetch).
- [ ] Retained position checkpoints bound a scrollable cursor's reread when it
  moves back (today it rereads from the first row).
- [ ] In object-store mode a join or grouping opens as a stream before it
  runs: the retained readers report the general executor's analysis errors
  and resume concurrent cold reads without stalling.
- [ ] Saving a held cursor at commit pauses on storage reads (the fourteen
  commit-path callers accept a wait before the commit is staged).

**Done when:**

- Statements past their first effect no longer block the worker on storage
  reads; the blocking list in the
  [execution model](docs/architecture.md#execution-model) is empty except
  where durable commit order requires it.
- Resumed work keeps its definitions, inputs, settings, effective user,
  command visibility, timestamps and deadline. Sequence calls, advisory locks,
  audit writes and delivered rows happen once, across repeated waits.
- A failed save leaves the live call usable; internal waits never trigger user
  error handlers; savepoint boundaries hold.
- Cancellation, timeout, rollback, disconnect and completion release saved
  state, inside startup pools (4,096-row results fit the 4 MiB statement
  arena).
- Tests with a delayed object-store GET show another client progressing, with
  empty caches, both transaction modes, both wire protocols, triggers and
  nested functions, and give PostgreSQL 18's rows, audit writes, sequence
  values, types, errors and rollback.

### Run SQL on several CPU workers

**Goal:** independent connections use several CPU cores. Each connection keeps
its command order, and every confirmed transaction stays recoverable.

**Remaining:**

- [ ] A fixed worker pool chosen before startup.
- [ ] Per-worker temporary execution state; shared rows, definitions, locks
  and transactions protected without a global statement lock.
- [ ] Database and account identity, fair queues, cancellation, storage waits
  and commit order preserved across workers.
- [ ] Overload returns explicit errors.

**Done when:** read, write and mixed workloads scale with workers and every
correctness gate passes with several workers.

### Close application compatibility gaps

**Goal:** applications that use supported PostgreSQL 18 features work
unchanged; anything else fails with an explicit error. This is the complete
known gap list, in priority order: finish work in progress, then fix results
that differ from PostgreSQL without an error, then implement features that
are refused today. A newly found gap goes into its group here or is fixed in
the PR that finds it.

The [compatibility guide](docs/postgresql-18-compatibility.md) and
[query planning guide](docs/query-planning.md) describe current behavior.
Fixture numbers refer to `tests/external/differential_exact/`.

#### 1. Finish work in progress

- [ ] **TOAST stored forms in routines.** PostgreSQL passes a stored
  (compressed or out-of-line) datum on unchanged. A function's result
  (PL/pgSQL `RETURN`, a SQL function's inlined body or result tuple, a folded
  IMMUTABLE call, a user-defined operator or cast), the arguments a
  user-defined cast, operator or aggregate transition function is passed,
  and a procedure's output parameters keep their forms (`998`). Still `0A000` where the form decides a size
  (`pg_column_size`):
  - `NEW` and `OLD` columns: PostgreSQL reads a `BEFORE` trigger's `NEW` as
    formed (3004 bytes) and an `AFTER` trigger's as stored (44); the firing
    path, the movable engine firings and queued `AFTER` events carry row
    values without forms;
  - a domain `CHECK`'s `VALUE`: the coercion does not carry its datum's form
    into the check;
  - an aggregate's transition state, its sorted or `DISTINCT` inputs, and
    its final function's arguments; an array constructor element's cast;
  - a SQL function body that returns a parameter without being inlined
    (PostgreSQL's plan cache folds it into a new datum or passes it on),
    and a SQL procedure's or data-modifying SQL function's result row.
- [ ] **TOAST stored forms in the executor:** the input datum `min`, `max`,
  `any_value`, `mode`, `percentile_disc` and window value functions return,
  grouping keys, scalar subqueries, window rows, index-only scans, `RETURNING`
  rows, a passed value's `varchar(n)` or `char(n)` length coercion, `lz4`, and
  compression of types other than text, `bytea`, `json`, `xml`, `tsvector`,
  `path` and `polygon`. `pg_column_size` of such a value, and `relpages`,
  `ANALYZE` or `CREATE INDEX` over a table storing it, are `0A000`; so are
  `EXPLAIN (ANALYZE, BUFFERS)` of writing a row TOAST compresses and
  `pg_column_size` of a short value an aggregate, window function or subquery
  passes on (PostgreSQL keeps its one-byte header).
- [ ] **Composite stored forms:** the one-byte header a short composite gets
  when a sort or materialized CTE forms it into a tuple, or when a stored one
  passes through a derived table, sort or materialized CTE (pos3ql reports the
  four-byte one); a derived table's whole-row output PostgreSQL pulls up; a
  composite column TOAST compresses as a whole (its header holds the row
  type's OID, which pos3ql assigns differently); a field of an array element
  (`(v[1]).a`) and the fields of whole rows `array_agg` collects; a dropped
  column's value in a stored row type that is not fixed-width by-value; an
  array of composites holding compressed fields inside a stored composite;
  binary logical replication of an array of a named composite type. Each is
  `0A000`.
- [ ] **Index sizes in `pg_class`.** `CREATE INDEX`, `REINDEX`, `VACUUM`,
  `ANALYZE` and `TRUNCATE` count an index's `relpages` and `reltuples` as
  PostgreSQL 18.6 does, from the btree its build lays out (`996`). Not yet:
  a btree grown by inserts has the pages PostgreSQL's page splits and
  deduplication leave (an index created before 6,000 inserts is costed and
  counted as built, 8.00 below PostgreSQL's full scan); a hash, GIN, GiST,
  SP-GiST or BRIN index over rows keeps the counts last measured (0 when
  never: only their empty layouts are modelled); `VACUUM` recounts entries
  once dead rows reach 2% of the table's pages, approximating the
  index-vacuum bypass; `ANALYZE` past its sample counts a partial index's
  entries exactly where PostgreSQL scales the sample's share; keys repeating
  under twice on average (`g % 1501` over 3,000 rows) lay out one page larger
  than `nbtsplitloc`'s choices.
- [ ] **Aggregate states and sorts:** transition states without a PostgreSQL
  size (hash aggregates over more than seven grouping columns; `json_agg`,
  `range_agg`, ordered-set aggregates, `array_agg` of arrays, `min`/`max` of
  arrays, records and bit strings: `0A000`); `GROUP BY` state streams instead
  of materializing; the modeled-join grouping sort and incremental sort
  batches are charged against `work_mem` through the `tuplesort` model; rows
  beyond the external sorter's first 256 KiB chunk sort ties in PostgreSQL's
  order (needs `work_mem`-sized run buffers and a streaming merge over up to
  500 tapes); a bounded sort outgrowing its heap under `EXPLAIN ANALYZE`
  (`0A000`).

#### 2. Results that differ without an error

**Query results and row order**

- [ ] Postponed select lists (`make_sort_input_target`): a volatile or
  routine-calling list runs after the sort and limit (it runs for every input
  row today).
- [ ] `WITH` queries: a query PostgreSQL runs once runs whole at its first
  read, and a recursive one read by a join runs an iteration at a time, so
  rows PostgreSQL never computes are evaluated (and can error); a join reads
  the whole query; a declared cursor runs it at its first `FETCH`; a reader
  past an earlier stop reruns it (`0A000` when volatile).
- [ ] Derived tables drop unread outputs (`remove_unused_subquery_outputs`):
  `SELECT * FROM t WHERE EXISTS (SELECT 1 FROM (SELECT 10 / t.x) s)` fails on
  a zero `x` today.
- [ ] Row order: equal-key rows reaching a window sort in quicksort order; a
  join's `DISTINCT` with an `ORDER BY` on some of its columns; tables whose
  rows vary in width arrive in PostgreSQL's free-space-map order.
- [ ] A short value a subquery PostgreSQL does not materialize passes on
  (`(SELECT a || '' x ... LIMIT 5) s`, a subquery without `FROM`) keeps its
  four-byte header; `pg_column_size` reports the packed one-byte form today.
- [ ] Parameter inference follows PostgreSQL's left-to-right analysis (some
  `42P18` and `42804` cases are accepted today).
- [ ] `ON CONFLICT DO NOTHING` without a target on a table with a deferrable
  constraint skips a row an earlier arbiter skips (`55000` today).

**Plans and `EXPLAIN` figures**

- [ ] Retire the legacy `EXPLAIN` path: a plan outside the PostgreSQL model
  either matches PostgreSQL or fails with `0A000`. Today `COSTS OFF` and
  `EXPLAIN ANALYZE` print pos3ql's own plan for such queries (hashing the
  second of two relations, nested loops for three or more, sorting before a
  join's `DISTINCT ... ORDER BY`, `HashAggregate` for a false condition, no
  flattened distinct union of more than two inputs, `Unique` before `Sort`,
  `top-N heapsort` reported as `external merge` once input spills).
- [ ] Join and general `GROUP BY` estimates apply statistics objects.
- [ ] Parenthesized inner joins follow PostgreSQL's join order instead of the
  written one (a cross product when an outer join is present or the first
  members share no condition: over 20 s for 400, 300 and 200 rows).
- [ ] Never-analyzed tables estimate as `estimate_rel_size` does (ten pages)
  and size their indexes as `CREATE INDEX` built them (an ordered index scan
  costs 20 too much on a text-keyed btree of 8,000 rows today).
- [ ] Stable expressions the model cannot evaluate (user functions) estimate
  as PostgreSQL does; the selectivity estimate's extra evaluation of a stable
  call compared with an indexed column is modelled.
- [ ] Heap placement after large updates (pruned pages, HOT chains).
- [ ] `BUFFERS`: an index scan read in part counts one page more than
  PostgreSQL; the visibility map clears every page after any change since
  `VACUUM`.
- [ ] Hash value types without a PostgreSQL size (arrays, `jsonb`, ranges,
  records) in a `Hash` node report pos3ql's bytes.
- [ ] Generic plan choice compares the generic plan's cost with the custom
  plans' average (`choose_custom_plan`); today it switches after five
  executions.
- [ ] Cumulative counters for `CREATE INDEX`'s heap scan, the heap fetches of
  bitmap and index-only scans (`idx_tup_fetch`), and partition index scans
  read only for their order.
- [ ] Range and multirange histograms; `compute_distinct_stats` for types
  without an ordering; the binary form of a composite value inside a
  statistics array (sent as text today).
- [ ] postgres_fdw: a foreign scan's printed cost (zero today); a join over a
  `WITH` query reading a foreign table, remote grouping with `DISTINCT`,
  windows over groups, `LIMIT` with a remote order by grouping keys,
  postponed volatile expressions and local `GROUP BY` under `LIMIT` follow
  PostgreSQL's costs.

**Messages, notices and positions**

- [ ] Notice and error positions: an `INSERT ... SELECT` output literal, an
  `ARRAY[...]` cast as a whole, PL/pgSQL statements, `''::tsquery` notices,
  `regclass` literal arguments (`nextval('nope')`), `CONSTANT` assignments,
  lexer-folded names and `$n` beyond a routine's parameters, `PREPARE p(t1) AS
  UPDATE t SET c = $1` assignment errors, and an unknown table in a select
  list over a derived table whose `EXISTS` reads the same name. A PL/pgSQL
  `CALL` whose argument holds a subquery positions the error in the statement
  (`QUERY:` with a `LINE` caret) under the `at CALL` context.
- [ ] A literal conversion's notice precedes folding errors (`UPDATE t SET a =
  1/0 WHERE ''::tsquery IS NOT NULL` omits the notice today); an untyped
  literal compared with a column (`WHERE q = ''`) converts once at analysis
  (its notice repeats per row today).
- [ ] Index key calls: a key whose calls nest restarts once per build, not
  per row; `REINDEX INDEX`, writes and table rewrites stop repeating the
  other indexes' key notices; a partial unique index's predicate and key
  calls interleave as in PostgreSQL.
- [ ] Parse converts the literals of `EXPLAIN`, `CREATE TABLE AS`, `CALL` and
  `DECLARE` and reports every analysis error at Parse (`SELECT current_date +
  $1`, `SELECT 1 WHERE 'x'` arrive at Bind or Execute today). Checks
  PostgreSQL makes in parse analysis (type modifiers, `COPY` options, storage
  parameters, sequence, database and large-object values, SQL/JSON result
  types) are reported there, in PostgreSQL's order.
- [ ] Column-not-found errors carry the `Perhaps you meant` hint.
- [ ] Wrong error codes: a whole-row reference (`SELECT t FROM t`) has the
  table's row type (`record` today, so `WHERE t = '(1,a)'` gives `42883`); a
  subquery's `t.*` naming an enclosing query's item (`42601`/`42P01`); `INSERT
  INTO t VALUES (1) UNION SELECT ...` (`42601`); a subquery reading `old` or
  `new` in `RETURNING` of a view write through an `INSTEAD OF` trigger or rule
  (`42P01`); a missing field of an anonymous row passed to a `record`
  parameter reports `record "r" has no field "a"` (`42P01`); renaming an enum
  value spelled in a SQL-standard body (`RETURN 'a'::e`) keeps the call
  working (`22P02`); `ALTER VIEW v DISABLE RULE r` (`42809`, `42601` today);
  heap storage parameters beyond `fillfactor` and `autovacuum_enabled`
  (`22023`); a parameter inside a `FROM` subquery of `PREPARE` is typed before
  the subquery is described (`42883`).
- [ ] PL/pgSQL: a `$n` beyond a routine's parameters in a statement that reads
  no rows is rejected at analysis; block label versus row variable resolution
  compares block by block; a rule created inside a routine words a missing
  `OLD.` column as PostgreSQL does; a SQL function whose earlier statements
  are not queries or data changes type-checks its final statement as
  PostgreSQL does.
- [ ] `DROP` messages: casts, operator classes and families, extensions,
  policies, triggers, rules, indexes and `DROP OWNED` describe dependents
  through the `pg_depend` walk, and `pg_event_trigger_dropped_objects` orders
  every statement's objects by it (a TOAST table's and a dropped column's
  order included); `DROP COLUMN` with inheritance children reports `cannot
  drop desired object(s)`; exclusion constraints join the shared `2BP01`
  report; `DROP ROLE`'s `DETAIL` lists `pg_shdepend` rows in physical order
  (needs a durable stamp per owner change).
- [ ] A GiST exclusion violation names the partner PostgreSQL's index scan
  finds first. The `index row size ... exceeds btree version 4 maximum 2704`
  error's DETAIL (`989`) names the row's tuple only while the table has had
  rows inserted and none updated, deleted, rolled back or vacuumed since its
  statistics were counted, and not for a row `ALTER TABLE` rewrites; an index
  build names the first oversized row in scan order where PostgreSQL uses key
  order. These need a heap position (`ctid`) model. A value under 511 bytes
  passed on compressed is sized as formed (PostgreSQL indexes the compressed
  datum as it is).

**Catalog contents**

- [ ] `pg_prepared_statements` lists protocol statements.
- [ ] `pg_type` lists `information_schema`'s view row types (needs the pinned
  reference image).
- [ ] Shipped `pg_description` text of built-in conversions and collations;
  built-in `pg_conversion` rows.
- [ ] `pg_type`, `pg_operator` and `pg_opfamily` return built-in rows in a
  fresh cluster's physical order.
- [ ] `pg_rewrite` lists built-in views' rules (145 `_RETURN` rules,
  `pg_settings_u`, `pg_settings_n`) and has `ev_qual` and `ev_action`.
- [ ] `pg_trigger` lists the internal `PK_ConstraintTrigger_*` and
  `Unique_ConstraintTrigger_*` rows; `pg_constraint` lists the system
  catalogs' not-null constraints.
- [ ] A new partition's equivalent `UNIQUE` or `PRIMARY KEY` constraint is
  attached to the parent's plain unique index instead of building a copy.
- [ ] `fillfactor` prints as written (`'070'`); view `reloptions` list in the
  order set, with an explicit `security_invoker=false`.

**Performance gaps**

- [ ] postgres_fdw join pushdown (`foreign_join_ok`) and estimates for join
  inputs (a foreign table joined with other relations, under column aliases
  or in a derived table below a join is read whole, and `EXPLAIN` of it fails
  with `0A000`); a SQL cursor or PL/pgSQL `FOR` loop over a foreign query
  fetches only the batches its reads need; a correlated subquery fetches only
  the columns it reads; InitPlans of `numeric`, character, time and interval
  types, correlated outer columns and a custom plan's argument values are
  shipped as parameters.
- [ ] `\dfS+` over 3,400 routines takes 0.5 s against PostgreSQL's 0.24 s
  (release builds, one host, 2026-11-13): resolve each call once while
  planning, and fold a routine's statement once per session.

#### 3. Features refused today

These fail with `0A000` (or the code shown) and need implementing.

**Built-in audit**

- [ ] Audit every PostgreSQL 18 built-in function by signature and executed
  behavior, in a checked-in list; a catalog entry is not support. A listed
  built-in that is neither implemented nor an operator, cast or input
  function fails with `0A000` today.
- [ ] Renaming or moving a built-in function changes its catalog entry: every
  built-in name lookup consults a renamed or moved row.

**Joins**

- [ ] Other `LATERAL` subqueries in the search: joins, grouping or volatile
  calls inside them, outer columns read outside their `WHERE`, a target list
  other than all their columns in order (a kept `Subquery Scan`), `LEFT JOIN
  LATERAL`, and queries filtering, grouping or ordering by their columns
  (`EXPLAIN` of them fails while `enable_memoize` is on).
- [ ] GEQO reclaims each tour's arena memory, as PostgreSQL frees a tour's
  memory context, so joins up to `max_join_relations` plan in the default
  statement arena (a 150-relation `USING` chain exhausts 32 MiB today, `54000`),
  and equivalence classes past 64 (a chain of more than 64 left joins) fit.
- [ ] Hash join keys of types without a ported hash function (`numeric`):
  plan them (the search avoids them, and a one-batch table that outgrows its
  memory fails).
- [ ] `GROUP BY` of an expression other than a full join's merged column
  (when costs print); conditions comparing expressions of several relations
  other than by a mergeable equality (`COALESCE(a.id, 0) < b.id`); an `OR` of
  join conditions under a semi or anti join (`clauselist_selectivity_or`
  under `eqjoinsel_semi`), and an `OR` join clause an indexed relation's
  parameterized scan evaluates (filter or `BitmapOr`); a constant equated with
  an outer join's column that another outer join nulls.
- [ ] Merge join over a partly sorted outer input (`Incremental Sort`), over
  an input in order that is not a plain unfiltered index scan under `EXPLAIN
  ANALYZE`, and a merge join's spilling sort.
- [ ] Derived tables kept as subqueries (`SubqueryScan`); a subquery reading
  `LATERAL` or outer columns is pulled up; a single-relation query drops a
  trivial `Subquery Scan`; a `LATERAL` subquery prints its own `Filter`; a
  join condition over a derived table names the `WITH` query's relation.
- [ ] `UPDATE ... FROM` and `DELETE ... USING` through the search (`EXPLAIN`
  of one that joins through an index); joined ones reaching foreign rows
  qualify them in the change loop; a routine reading a joined column is
  costed as PostgreSQL does.
- [ ] Parenthesized and `USING` joins: a merged column needing a coercion
  (`int` with `bigint`) or merging more than two columns, a member named as a
  later FROM item, a whole-row reference to the join, a `USING` join nested in
  a parenthesized join inside another, `USING ... AS`, nested aliased joins.
- [ ] Joins outside the search with costs (`810`), including `INSERT`s whose
  values need uncosted coercions, `INSERT` into a view, a write whose source
  the planner does not model, a rule action of several `VALUES` rows, a
  coerced `INSERT` source value and a subquery in `ON CONFLICT`.

**Other plans and `EXPLAIN`**

- [ ] Single-table index paths: non-btree access methods; expression indexes
  calling a user routine or type after inlining; set-returning SQL functions
  inlined with a `LATERAL` argument; window `PARTITION BY` over an
  expression; regular expressions with embedded options or a director
  (`(?i)`, `***=`); statistics-object MCV lists of other types than those
  estimated today; sub-partitioned tables whose partitions have index paths;
  `ORDER BY` of an index expression of a tree; a tree with a foreign member
  beside indexed members; `Index Searches` of array or skip-array scans over
  several leaf pages.
- [ ] Single-table aggregate costing for `DISTINCT`/`ORDER BY` aggregate
  inputs, user-defined aggregates, expressions over aggregates, `ORDER BY` of
  an aggregate and grouping by expressions (pos3ql's estimate today, without
  an error); grouping sets outside the cost model.
- [ ] `BUFFERS` of scans of the legacy executor and subplans without a page
  model, of hash joins in batches, and of a scan or InitPlan running a
  subplan.
- [ ] Windows: window functions inside expressions or the `ORDER BY`; windows
  over one relation's groups; merging clauses equal after
  `optimize_window_clauses`; run conditions (`find_window_run_conditions`); a
  window sort's `Memory` and tuplestore `Maximum Storage`; `EXPLAIN ANALYZE`
  of windows over an index scan, under a limit or over a join's groups, and of
  frames and functions beyond the default and `ROWS UNBOUNDED PRECEDING`
  frames, `row_number`, `rank`, `dense_rank` and plain aggregates.
- [ ] Presorted aggregates' sort under `EXPLAIN ANALYZE`.
- [ ] Hash aggregates: `aset.c`'s recycled executor and transition contexts
  after a session's hash aggregate under `work_mem` below 1MB (from the next
  statement on while the bucket array is within 8 kB or a transition value
  takes more than 1 kB); `EXPLAIN ANALYZE` of one that spills rows holding
  columns besides its keys, or whose group estimate lies outside the cost
  model.
- [ ] Unique index builds PostgreSQL would sort in parallel or beyond 64 MB
  `maintenance_work_mem` (on duplicates).
- [ ] Subplans: a correlated subplan in an `OR` or compared with a column;
  row subqueries as subplans; a correlated subquery of a join; group lines of
  a window's own incremental sort or one under a `LIMIT`; `ANALYZE` figures
  of an `ALL` subplan's `Materialize`; a subquery reading a relation named
  like the query's, or a second reading the same relation (PostgreSQL names
  it `t_1`); hashing an expression key or joining unique subquery rows
  instead of probing an index per row.
- [ ] `WITH` queries in plans: a materialized `WITH` inside a subquery reads
  enclosing columns (`42P01` today); a `CTE Scan` under a scalar subquery's
  InitPlan, and a subplan reading an enclosing materialized `WITH`, print
  figures instead of `(never executed)`; a data-modifying `WITH`'s `INSERT
  ... SELECT` prints `ANALYZE` figures; an `IN` over a subquery whose `WITH`
  runs once is pulled up.
- [ ] `VERBOSE` output lists: joined `UPDATE`/`DELETE` other than one FROM
  table read sequentially; `MERGE` from `VALUES` with `NOT MATCHED BY SOURCE`
  or with subqueries in its lists; several-row `INSERT`s omitting columns or
  using `DEFAULT`; defaulted identity, serial or generated columns; `INSERT
  ... SELECT ... RETURNING`; `INSERT` into a view; `(record).*` items; a sort
  below a postponed select list; an `EXISTS` subplan's list. Also per-node
  `WAL`, trigger time lines, and `Heap Fetches`/`Heap Blocks` on the legacy
  path.
- [ ] Partitioned tables and inheritance trees: a member index on a column
  the query reads; `EXPLAIN ANALYZE` of pruning by a subquery reading an
  enclosing query; writes with a correlated, set or row subquery or one in
  `RETURNING`; full or anti join with an emptied partitioned table, outer join
  with an emptied inheritance tree; a foreign member shipping only some
  conditions; `VERBOSE` over foreign members; whole-row references to a child
  with its own columns. Joins and grouping over them use PostgreSQL's
  methods; an erroring `WHERE` conjunct runs at its scan; a hash join with an
  empty build reads at most one outer row; a derived table pulled into an
  outer join prints under PostgreSQL's name.
- [ ] `MERGE` plans for sources without statistics (PostgreSQL merges them),
  sources other than a table or `VALUES`, indexes on other columns `ON` reads
  or on the outer relation's keys, partial or expression indexes, and row
  security.
- [ ] `ON CONFLICT DO UPDATE ... WHERE` (`Conflict Filter`) in `EXPLAIN`;
  inference `WHERE` proves `OR`, `IN` and non-integer ranges as
  `predicate_implied_by` does.
- [ ] Constant folding of aggregate arguments, window specifications, `FROM`
  subqueries, `MERGE` into a view, join conditions inside a parenthesized join
  and custom plan parameters.

**Rows, records and routines**

- [ ] Row constructors cast to a view's row type; arrays of a table's row type
  (`ARRAY[ROW(1, 'a')::t]`).
- [ ] Table and view row types in routine signatures (`CREATE FUNCTION
  f(orders)`); PL/pgSQL variables of a view's row type and `%TYPE` of a view
  column.
- [ ] A scalar function as a `FROM` item (`FROM t, nextval('s') x`).
- [ ] PL/pgSQL `WITH ... INSERT/UPDATE/DELETE` statements, and PL/pgSQL
  writes with `WITH` queries whose nested `AFTER` trigger waits.
- [ ] Routine variables in a static write starting with `WITH`, a
  materialized `WITH` inside a derived table, and an ordinary `WITH` read by a
  data-modifying one (`42703` today): pass the routine's row as
  `expand_plpgsql_with` does.
- [ ] A trigger fired inside a write can `FETCH` from a session cursor that
  calls a user routine per row.
- [ ] A `READ COMMITTED` recheck of a row joined through a `LATERAL` subquery
  or a `UNION ALL` branch that reads tables.
- [ ] Rule actions reading other relations, and `INSERT`s whose query has
  volatile output, stream their derived relation; an `INSERT ... SELECT` whose
  condition and output both call routines interleaves them per row.

**Schema objects**

- [ ] Bind statistics expressions, trigger `WHEN` conditions and domain
  constraints to the objects they name, so renaming or moving a named routine
  or type, or renaming an enum value they cast to, works.
- [ ] Renaming a column a rule action names without `NEW.`/`OLD.`.
- [ ] Deparse subqueries, named-argument calls, `$n` and array constructors in
  stored expressions; non-`SELECT` statements of `BEGIN ATOMIC` bodies; policy
  expressions; rule actions with data-modifying `WITH`; `FROM` functions with
  column definition lists beyond `json[b]_to_record[set]`.
- [ ] `ALTER COLUMN ... TYPE` of columns that views, rules, trigger `WHEN`
  conditions, policies, SQL-standard bodies or stored writes read.
- [ ] Cascading drops of a view whose row type another column, attribute or
  domain uses.
- [ ] Partition keys that are expressions or carry `COLLATE`; `HASH` keys of
  other types.
- [ ] `CHECK ... NO INHERIT`; exclusion element operator class options.
- [ ] `COPY FROM` into a view with an `INSTEAD OF INSERT` trigger; a refused
  `COPY FROM` into a view or sequence validates its options first.
- [ ] `COPY` to or from a server file or program, `COPY ... FREEZE`, and
  `ENCODING` other than UTF8: implement, or record each as an architecture
  boundary in the compatibility guide.

**Logical replication and postgres_fdw**

- [ ] Per-table initial copy with PostgreSQL's `d`, `f` and `s` states (one
  bootstrap under one synchronization slot today).
- [ ] `ALTER SUBSCRIPTION ... REFRESH PUBLICATION` rejects a table published
  with different column lists in the statement, not in its copy.
- [ ] Publisher connection strings take libpq's defaults for port, user,
  dbname and sslmode (`22023` today). Host names are blocked
  ([B-351](BUGS.md)).
- [ ] libpq options beyond host, hostaddr, port, dbname, user, password,
  connect_timeout, application_name and sslmode (`HV00D`).

**Done when:** every item above is closed or fails with an explicit error
recorded in the compatibility guide, the built-in audit list covers every
PostgreSQL 18 built-in, and the client suites, differential corpora and
regression schedule pass on every native target.

### Share fixed capacity by demand

**Goal:** fixed startup pools of memory and disk follow current demand instead
of private per-table reservations. [Architecture](docs/architecture.md#shared-memory-pools)
lists the shared pools today.

**Remaining:**

- [ ] A transaction writes more rows than `row_overlay_slots`, `txn_rows` or
  `table_metadata_leases` hold: its uncommitted rows and prepared index keys
  move out of memory (a 10,000-row `INSERT` fails with `54000` under the
  8,192 defaults today, as no checkpoint can free its own rows).
- [ ] Local mode (`object_store = off`) saves checkpointed rows outside
  memory, so its tables together can hold more rows than
  `row_overlay_slots`.
- [ ] A write that cannot pause (`COPY FROM`, a statement after its first
  unreplayable effect) waits for a checkpoint when `row_overlay_slots` or
  `row_history_slots` are exhausted, without a restart (`54000` today).
- [ ] Reserve forward progress for commit, rollback, required reads and
  maintenance; reclaim in bounded steps; report pinned capacity and named
  exhaustion; reserve replacements before effects (if analyzing `orders`
  cannot reserve statistics space, the old statistics stay usable).
- [ ] Reduce the remaining catalog reservations from measured component
  costs.
- [ ] Commit-time catalog history captures only the rows a change touches:
  `pg_type`, `pg_shdepend`, `pg_description`, `pg_publication_rel`,
  `pg_policy`, `pg_statistic_ext`, `pg_partitioned_table`, `pg_rewrite` and
  `pg_sequence` still materialize every table's rows, and changes to views,
  sequences, routines and types select every table. Also: a table's index
  lookup scans every index, and a foreign key finds its parent by name.
- [ ] Decide whether cache files need guaranteed disk reservation, then
  implement and verify it (file length is not reserved space).
- [ ] Measure 1, 8, 32 and 128 tables at equal explicit capacity in both
  modes, narrow and wide schemas, with retained readers and schema and index
  replacement; label every counter and unit; separate startup reservations,
  resident pages, stack and TLS. Compare with the
  [historical embedded measurements](https://github.com/e6qu/pos3ql/blob/4054a6d3c79e916728f8e84d84139eae6d86b501/benchmarks/baselines/2026-10-30-embedded-lazy-definitions/README.md)
  only at identical capacities.

**Done when:** total reserved memory is unchanged, capacity is conserved, no
runtime heap growth occurs, release after errors is safe, and the
measurement matrix supports any claimed embedded minimum.

### Qualify production deployment

**Goal:** a user can choose hardware and trust failover from published
evidence.

**Remaining:**

- [ ] A documented, tested replacement procedure from object storage that
  reroutes clients.
- [ ] Extend the baseline to useful data sizes, joins, large catalogs,
  maintenance, several workers, logical replicas, delays, recovery and
  multi-host failover with real client routing.
- [ ] Run each scenario repeatedly on pinned hardware with independent object
  storage and both PostgreSQL controls.

**Done when:** published measurements support stated sizes, transaction rates,
bandwidth and failover times. Calculated estimates and theoretical address
limits do not count.

## Upkeep

- [ ] The two macOS arm64 library test suites each peak within about 0.1 GB
  of the hosted runner's 7 GB; split them further (for example storage and
  checkpoint units apart from SQL units) before the next large addition. See
  [CONTRIBUTING](CONTRIBUTING.md#time-budget-and-sharding).

## After the finish line

This milestone is non-blocking: it is not part of the finish-line conditions
and nothing else depends on it. It needs measurements that do not exist yet,
so it starts after the baseline and capacity sharing.

### Adapt batching and layout with feedback control

**Goal:** inside fixed limits, pos3ql adjusts batch sizes, admission and future
data layout to the observed workload with measured feedback control. The
[resource design](docs/adaptive-resources.md) holds the proposal.

**Remaining:**

- [ ] Adjust active windows, lease ownership and batch sizes inside the fixed
  request slots, arenas and disk limits; bound queue delay and bytes; protect
  demand reads, commit publication and required maintenance before read-ahead
  or duplicate requests.
- [ ] Compare simple feedback, proportional-integral (PI) and
  proportional-integral-derivative (PID) controllers, each with an explicit
  policy, ceilings, minimum progress, sample interval and count, and maximum
  batch delay and bytes; clamp changes, filter noisy samples, avoid repeated
  reversals and stop integral error from accumulating when owned capacity
  cannot be reclaimed (anti-windup). Choose from measurements.
- [ ] Choose row or column output, and ranged or whole-object reads, from
  measured column demand, reuse, update rate and rewrite cost, without
  rewriting layout after short bursts.

**Done when:** fixed and adaptive policies are compared at equal resources on
MinIO and SeaweedFS against both PostgreSQL controls, across idle, burst,
sustained and cooling traffic, point and scan mixes, bulk writes, delays,
failures, cancellation, long readers and maintenance pressure, reporting
latency distributions, throughput, requests and bytes, useful prefetch, cache
and pinned usage, fairness and controller settling time; correctness, durable
order and startup limits hold throughout.

## Working rules

[AGENTS.md](AGENTS.md) holds the engineering and change rules and
[CONTRIBUTING.md](CONTRIBUTING.md) the verification gates.
