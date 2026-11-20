# Finish pos3ql

[Overview](README.md) · [Architecture](docs/architecture.md) ·
[What works today](docs/implemented-baseline.md) ·
[Limits](docs/postgresql-18-compatibility.md) · [Contributing](CONTRIBUTING.md)

This plan lists only unfinished work, as checklists. Current behavior belongs
in the [guides](docs/README.md); historical plans are in the
[history index](docs/history/README.md). An item is removed when its PR
merges; a milestone is removed when its "done when" is demonstrated.

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
logical replication, backup and recovery from object storage work today and are
compared with PostgreSQL 18 in CI. Object-store requests overlap and commits
share uploads. SQL runs on one CPU worker. Reads, cursors, routines, triggers
and the row selection of writes pause during a storage wait; a write's later
phases, DDL, `COPY` and replication apply still block the worker. No
measurement on separate machines has been published.

## Milestones

Milestones ship in complete PRs, in this order. Compatibility work runs
alongside the others. The last milestone is non-blocking (see
[After the finish line](#after-the-finish-line)).

| Milestone | Status | Depends on |
|---|---|---|
| [Pause and resume every query path](#pause-and-resume-every-query-path) | In progress | — |
| [Isolate readers and bound cleanup](#isolate-readers-and-bound-cleanup) | In progress | — |
| [Measure a first deployment baseline](#measure-a-first-deployment-baseline) | Not started; next | — |
| [Close application compatibility gaps](#close-application-compatibility-gaps) | In progress | — |
| [Share fixed capacity by demand](#share-fixed-capacity-by-demand) | Partly done | — |
| [Run SQL on several CPU workers](#run-sql-on-several-cpu-workers) | Not started | Pausing, reader isolation |
| [Qualify production deployment](#qualify-production-deployment) | Not started | All of the above |
| [Adapt batching and layout with feedback control](#adapt-batching-and-layout-with-feedback-control) | Designed; non-blocking | Baseline, capacity sharing |

The baseline comes early because it tests the project's core bet: that
commits through object storage give useful latency and cost. Its results may
reorder the later milestones.

### Pause and resume every query path

**Goal:** a statement waiting for object storage or another connection's lock
gives up the CPU worker and later resumes exactly where it stopped. The
[execution model](docs/architecture.md#execution-model) describes what pauses
today.

**Remaining:**

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
  record of their enclosing write's attempt loop.
- [ ] A data-modifying `WITH` main statement after its sub-statements pauses.
- [ ] A join reader of a data-modifying `WITH` query pulls it as PostgreSQL's
  plan does (a nested loop under `LATERAL`, or a hash join that hashes the
  other input) instead of reading it whole at its first read; needs a retained
  join node that pulls the query as its outer input.
- [ ] `INSTEAD OF` view rows, DDL, `COPY`, replication apply and deferred
  triggers pause on storage reads.
- [ ] Row progress for data-modifying `WITH` inside routine bodies, so a
  routine call after their first row change no longer fails with `0A000`
  (`SQL row expression requires retained write progress`).
- [ ] `INSERT ... SELECT` from a source calling user routines streams with
  per-row result storage released once the row is written (today it
  materializes; streaming would exhaust the statement arena, `54000`).
- [ ] Calls in a materialized `INSERT ... SELECT` source's `WHERE`, join
  conditions, subqueries, aggregate arguments or set-operation branches run
  per row, so they see rows the statement inserts.
- [ ] More retained shapes, by the
  [retained plan tree design](docs/architecture.md#retained-plan-tree):
  index-probed joins; `RIGHT` and `FULL` joins of more than two items or over
  generator calls; grouping sets; correlated and grouped or bounded
  subqueries (rewinding a child tree per outer row instead of re-analysing);
  set operations other than `UNION ALL` (and `UNION ALL` under `ORDER BY`, in
  a declared cursor, or with volatile or routine-calling expressions);
  set-returning functions with non-constant arguments, user-defined ones and
  column definition lists (`jsonb_to_recordset(...) AS t(a int)`).
- [ ] Object-store sorts stream through a run reader per stream (today a
  large ordered `SELECT` there spools, and a paged portal over one buffers in
  `portal_result_bytes`).
- [ ] Declared cursors use the retained sort; cursors over the retained
  shapes read as they fetch instead of running the query at the first read
  (a scrollable cursor ordered by a non-column expression re-evaluates
  volatile output; a row-locking cursor over a join locks per fetch).
- [ ] Retained position checkpoints bound a scrollable cursor's reread when it
  moves back (today it rereads from the first row).
- [ ] In object-store mode a join or grouping opens as a stream before it
  runs: the retained readers report the general executor's analysis errors
  and resume concurrent cold reads without stalling.
- [ ] Saving a held cursor at commit pauses on storage reads (the fourteen
  commit-path callers accept a wait before the commit is staged).

**Done when:**

- Writing statements and statements past their first effect no longer block
  the worker on storage reads; the blocking list in the
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

### Isolate readers and bound cleanup

**Goal:** a reader finishes with the rows and definitions it selected while
schema changes, commits and maintenance continue; cleanup never removes
anything a reader or backup still needs.

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

### Measure a first deployment baseline

**Goal:** test whether commits through object storage give useful latency and
cost compared with PostgreSQL.

**Remaining:**

- [ ] Record two fixed hosts, the network and separate object storage,
  without credentials.
- [ ] Run the [measurement suite](docs/performance.md) repeatedly against both
  PostgreSQL 18 controls: one with the host's resources, one with pos3ql's CPU
  and memory allowance.
- [ ] Report typical and slow commit latency, point-read and scan latency with
  warm and empty caches, and object requests and bytes per transaction.
- [ ] Keep revisions, raw results, latency distributions, CPU and RSS, request
  counts, bytes and cost inputs in [benchmarks](benchmarks/README.md).

**Done when:** a dated, reproducible baseline is published and the milestone
order below has been reviewed against it.

### Close application compatibility gaps

**Goal:** applications that use supported PostgreSQL 18 features work
unchanged; anything else fails with an explicit error. The list below is the
complete known gap list. A newly found gap is added here or fixed in the PR
that finds it.

The [compatibility guide](docs/postgresql-18-compatibility.md) and
[query planning guide](docs/query-planning.md) describe current behavior.
Fixture numbers refer to `tests/external/differential_exact/`.

#### Built-in audit

- [ ] Audit every PostgreSQL 18 built-in function by signature and executed
  behavior, in a checked-in list; a catalog entry is not support. A listed
  built-in that is neither implemented nor an operator, cast or input
  function fails with `0A000` today.
- [ ] Renaming or moving a built-in function changes its catalog entry
  (`0A000` today): every built-in name lookup consults a renamed or moved row.

#### Join search (joins work)

- [x] Full join merged keys: a `COALESCE` of a full join's compared columns
  equated with a constant (PR #779, `975`).
- [x] Self-join removal (`remove_useless_self_joins`, `976`).
- [ ] *In progress:* plan joins of never-analyzed tables by PostgreSQL's
  search (they keep pos3ql's join order today). Needs the retained pull-based
  executor below so the result-stream and cursor tests keep streaming.
- [ ] *In progress:* a retained pull-based executor whose merge join, sort,
  hash and hash aggregate nodes keep their state between rows, as the retained
  nested-loop and hash levels do.
- [ ] *In progress:* `LATERAL` subqueries planned by the search, with
  `Memoize` and exact `EXPLAIN ANALYZE` counters (`EXPLAIN` fails with
  `0A000` while `enable_memoize` is on today).
- [ ] *In progress:* relation sets sized at startup, so queries of more than
  128 relations are searched (they run pos3ql's join order today), and an
  indexed relation equated with more than 16 others no longer fails with
  `54000`.
- [ ] *In progress:* `ORDER BY` over a large join streams the joined rows into
  the retained, spilling sort (seven 8-row tables cross joined with `ORDER BY
  1 LIMIT 1` fails with `54000` today).
- [ ] `GROUP BY` of an expression other than a full join's merged column
  (`0A000` when costs print).
- [ ] Conditions comparing expressions of several relations other than by a
  mergeable equality (`COALESCE(a.id, 0) < b.id`).
- [ ] An `OR` of join conditions under a semi or anti join
  (`clauselist_selectivity_or` under `eqjoinsel_semi`), and an `OR` join
  clause an indexed relation's parameterized scan evaluates (filter or
  `BitmapOr`).
- [ ] Merge join over a partly sorted outer input (`Incremental Sort`).
- [ ] Derived tables kept as subqueries (`SubqueryScan`).
- [ ] A constant equated with an outer join's column that another outer join
  nulls.
- [ ] `UPDATE ... FROM` and `DELETE ... USING` through the search (`EXPLAIN`
  of one that joins through an index fails with `0A000`).
- [ ] A correlated subplan in an `OR` or compared with a column (`0A000`), and
  `BUFFERS` of a scan or InitPlan running a subplan (`0A000`).
- [ ] Parenthesized and `USING` joins: a merged column needing a coercion
  (`int` with `bigint`) or merging more than two columns, a member named as a
  later FROM item, a whole-row reference to the join, a `USING` join nested in
  a parenthesized join inside another, `USING ... AS`, nested aliased joins
  (each `0A000`).
- [ ] Parenthesized inner joins the search does not plan join in written
  order, leaving a cross product when an outer join is present or the first
  members share no condition (over 20 s for 400, 300 and 200 rows): use
  PostgreSQL's join order.
- [ ] Hash join keys of types without a ported hash function (`numeric`):
  plan them (today the search avoids them, and a one-batch table that
  outgrows its memory fails with `0A000`).
- [ ] Join and general `GROUP BY` estimates apply statistics objects (ignored
  today, without an error).
- [ ] Run `tests/external/join_capacity_diff.py` (analyzed 34- and
  128-relation `USING` chains under GEQO) in CI.

#### Plans outside the join search

- [ ] Retire the legacy `EXPLAIN` path: a plan outside the PostgreSQL model
  either matches PostgreSQL or fails with `0A000`. Today `COSTS OFF` and
  `EXPLAIN ANALYZE` print pos3ql's own plan for such queries (hashing the
  second of two relations, nested loops for three or more, sorting before a
  join's `DISTINCT ... ORDER BY`, `HashAggregate` for a false condition, no flattened
  distinct union of more than two inputs, `Unique` before `Sort`,
  `top-N heapsort` reported as `external merge` once input spills).
- [ ] Single-table index paths, each `0A000` in `EXPLAIN` today: non-btree
  access methods; expression indexes calling a user routine or type after
  inlining; set-returning SQL functions inlined with a `LATERAL` argument;
  window `PARTITION BY` over an expression; regular expressions with embedded
  options or a director (`(?i)`, `***=`); statistics-object MCV lists of
  other types than those estimated today; sub-partitioned tables whose
  partitions have index paths; `ORDER BY` of an index expression of a tree; a
  tree with a foreign member beside indexed members; `Index Searches` of array
  or skip-array scans over several leaf pages.
- [ ] Single-table aggregate costing for `DISTINCT`/`ORDER BY` aggregate
  inputs, user-defined aggregates, expressions over aggregates, `ORDER BY` of
  an aggregate and grouping by expressions (pos3ql's estimate today); grouping
  sets outside the cost model.
- [ ] Never-analyzed tables: estimate as `estimate_rel_size` does (ten pages)
  and size their indexes as `CREATE INDEX` built them (an ordered index scan
  costs 20 too much on a text-keyed btree of 8,000 rows today).
- [ ] Stable expressions the model cannot evaluate (user functions) estimate
  as PostgreSQL does.
- [ ] Btree sizes and heap placement: count pages as `nbtsort` and
  `nbtsplitloc` make them (keys repeating under twice on average, as `g %
  1501` over 3,000 rows, lay out one page larger), model heap placement after
  large updates (pruned pages, HOT chains), and report an index's
  `pg_class` pages and tuples. Today these are inexact without an error.
- [ ] `BUFFERS`: an index scan read in part counts one page more than
  PostgreSQL; the visibility map clears every page after any change since
  `VACUUM`; scans of the legacy executor and subplans without a page model,
  and hash joins in batches, fail with `0A000`.
- [ ] Windows: window functions inside expressions or the `ORDER BY`; windows
  over one relation's groups; merging clauses equal after
  `optimize_window_clauses`; run conditions (`find_window_run_conditions`); a
  window sort's `Memory` and tuplestore `Maximum Storage`; `EXPLAIN ANALYZE`
  of windows over an index scan, under a limit or over a join's groups, and of
  frames and functions beyond the default and `ROWS UNBOUNDED PRECEDING`
  frames, `row_number`, `rank`, `dense_rank` and plain aggregates (each
  `0A000`); equal-key rows reaching a window sort in quicksort order.
- [ ] Presorted aggregates' sort under `EXPLAIN ANALYZE` (`0A000`).
- [ ] Subplans: row subqueries as subplans; a correlated subquery of a join;
  group lines of a window's own incremental sort or one under a `LIMIT`;
  `ANALYZE` figures of an `ALL` subplan's `Materialize`; a subquery reading a
  relation named like the query's, or a second reading the same relation
  (PostgreSQL names it `t_1`); hashing an expression key or joining unique
  subquery rows instead of probing an index per row.
- [ ] Postponed select lists (`make_sort_input_target`): evaluate a volatile
  or routine-calling list after the sort and limit (it runs for every input
  row today).
- [ ] `VERBOSE` output lists (`0A000` today): joined `UPDATE`/`DELETE` other
  than one FROM table read sequentially; `MERGE` from `VALUES` with `NOT
  MATCHED BY SOURCE` or with subqueries in its lists; several-row `INSERT`s
  omitting columns or using `DEFAULT`; defaulted identity, serial or
  generated columns; `INSERT ... SELECT ... RETURNING`; `INSERT` into a view;
  `(record).*` items; a sort below a postponed select list; an `EXISTS`
  subplan's list. Also per-node `WAL`, trigger time lines, and `Heap
  Fetches`/`Heap Blocks` on the legacy path.
- [ ] `WITH` queries: a query PostgreSQL runs once runs whole at first read,
  and a recursive one read by a join runs an iteration at a time, so rows
  PostgreSQL never computes are evaluated (and can error); a join reads the
  whole query; a declared cursor runs it at its first `FETCH`; a reader past
  an earlier stop reruns it (`0A000` when volatile); a materialized `WITH`
  inside a subquery cannot read enclosing columns (`42P01`); a `CTE Scan`
  under a scalar subquery's InitPlan, and a subplan reading an enclosing
  materialized `WITH`, print `(never executed)`; a data-modifying `WITH`'s
  `INSERT ... SELECT` prints no `ANALYZE` figures; an `IN` over a subquery
  whose `WITH` runs once stays a subplan.
- [ ] Derived tables: a subquery reading `LATERAL` or outer columns is pulled
  up; a single-relation query drops a trivial `Subquery Scan`; a `LATERAL`
  subquery prints its own `Filter`; a join condition over a derived table
  names the `WITH` query's relation; unread outputs are removed
  (`remove_unused_subquery_outputs`; `SELECT * FROM t WHERE EXISTS (SELECT 1
  FROM (SELECT 10 / t.x) s)` fails on a zero `x` today).
- [ ] Partitioned tables and inheritance trees, each `0A000` today: a member
  index on a column the query reads; `EXPLAIN ANALYZE` of pruning by a
  subquery reading an enclosing query; writes with a correlated, set or row
  subquery or one in `RETURNING`; full or anti join with an emptied
  partitioned table, outer join with an emptied inheritance tree; a foreign
  member shipping only some conditions; `VERBOSE` over foreign members;
  whole-row references to a child with its own columns. Joins and grouping
  over them use PostgreSQL's methods; an erroring `WHERE` conjunct runs at its
  scan; a hash join with an empty build reads at most one outer row; a derived
  table pulled into an outer join prints under PostgreSQL's name.
- [ ] `MERGE` plans for sources without statistics (PostgreSQL merges them),
  sources other than a table or `VALUES`, indexes on other columns `ON` reads
  or on the outer relation's keys, partial or expression indexes, and row
  security (`EXPLAIN` fails with `0A000` today).
- [ ] Joins outside the search with costs (`0A000` today, `810`), including
  `INSERT`s whose values need uncosted coercions, `INSERT` into a view, a
  write whose source the planner does not model, a rule action of several
  `VALUES` rows, a coerced `INSERT` source value and a subquery in
  `ON CONFLICT`.
- [ ] `ON CONFLICT DO UPDATE ... WHERE` (`Conflict Filter`) in `EXPLAIN`
  (`0A000`); inference `WHERE` proves `OR`, `IN` and non-integer ranges as
  `predicate_implied_by` does.
- [ ] Merge join over an input in order that is not a plain unfiltered index
  scan under `EXPLAIN ANALYZE`, and a merge join's spilling sort (`0A000`).
- [ ] Generic plan choice compares the generic plan's cost with the custom
  plans' average (`choose_custom_plan`); today it switches after five
  executions.
- [ ] `\dfS+` over 3,400 routines takes 0.5 s against PostgreSQL's 0.24 s
  (release builds, one host, 2026-11-13): resolve each call once while
  planning.

#### Sorting, hashing and grouping state

- [ ] *In progress:* aggregate transition states without a PostgreSQL size:
  size them, so a join's hashed `GROUP BY` that spills and `EXPLAIN ANALYZE`
  of hash aggregates with `numeric` sums, text `min`/`max`, collecting
  aggregates or more than seven grouping columns stop failing with `0A000`.
- [ ] *In progress:* `GROUP BY` state streams instead of materializing.
- [ ] *In progress:* route every sort through the `tuplesort` model: the
  modeled-join grouping sort and incremental sort batches are charged against
  `work_mem`.
- [ ] *In progress:* multi-chunk external sort: rows beyond the external
  sorter's first 256 KiB chunk sort ties in PostgreSQL's order (needs
  `work_mem`-sized run buffers and a streaming merge over up to 500 tapes); a
  bounded sort outgrowing its heap under `EXPLAIN ANALYZE` (`0A000`).
- [ ] *In progress:* partition seek: a partition member is read through its
  own index as PostgreSQL's costs choose.
- [ ] *In progress:* pglz compression and TOAST sizes, so hash, sort and
  statistics figures match for values PostgreSQL compresses or stores out of
  line (rows over about 2 kB).
- [ ] Hash aggregates inherit `aset.c`'s recycled executor context after a
  session's hash aggregate under `work_mem` below 1MB (`0A000` from the next
  statement on while the bucket array is within 8 kB).
- [ ] `EXPLAIN ANALYZE` of a hash aggregate that spills rows holding columns
  besides its keys, or whose group estimate lies outside the cost model
  (`0A000`).
- [ ] Enum grouping order: enum OIDs come from pos3ql's identity band, so a
  hashed enum grouping's order cannot match a PostgreSQL cluster's (`925`).
- [ ] A join's `DISTINCT` with an `ORDER BY` on some of its columns, and the arrival
  order of tables whose rows vary in width (PostgreSQL's free space map).
- [ ] Unique index builds PostgreSQL would sort in parallel or beyond 64 MB
  `maintenance_work_mem` (`0A000` on duplicates).

#### Statistics

- [ ] Range and multirange histograms; `compute_distinct_stats` for types
  without an ordering; widths of compressed values; the binary form of a
  composite value inside a statistics array (sent as text today).
- [ ] Hash value types without a PostgreSQL size (arrays, `jsonb`, ranges,
  records) in a `Hash` node report pos3ql's bytes.

#### Constant folding, literals and positions

- [ ] Fold aggregate arguments, window specifications, `FROM` subqueries,
  `MERGE` into a view, join conditions inside a parenthesized join and custom
  plan parameters; fold a routine's statement once per session.
- [ ] Model the selectivity estimate's extra evaluation of a stable call
  compared with an indexed column.
- [ ] Convert an untyped literal compared with a column (`WHERE q = ''`) once
  at analysis (its notice repeats per row today).
- [ ] Parse converts the literals of `EXPLAIN`, `CREATE TABLE AS`, `CALL` and
  `DECLARE` at Parse, and reports every analysis error there (`SELECT
  current_date + $1`, `SELECT 1 WHERE 'x'` arrive at Bind or Execute today);
  `pg_prepared_statements` lists protocol statements.
- [ ] Notice and error positions: an `INSERT ... SELECT` output literal, an
  `ARRAY[...]` cast as a whole, PL/pgSQL statements, `''::tsquery` notices,
  `regclass` literal arguments (`nextval('nope')`), `CONSTANT` assignments,
  lexer-folded names and `$n` beyond a routine's parameters, `PREPARE p(t1) AS
  UPDATE t SET c = $1` assignment errors, and an unknown table in a select
  list over a derived table whose `EXISTS` reads the same name.
- [ ] A literal conversion's notice precedes folding errors (`UPDATE t SET a =
  1/0 WHERE ''::tsquery IS NOT NULL` omits the notice today).
- [ ] Checks PostgreSQL makes in parse analysis (type modifiers, `COPY`
  options, storage parameters, sequence, database and large-object values,
  SQL/JSON result types) are reported at analysis, in PostgreSQL's order.
- [ ] Column-not-found errors carry the `Perhaps you meant` hint.
- [ ] Parameter inference follows PostgreSQL's left-to-right analysis (`42P18`
  and `42804` cases are accepted today).
- [ ] A parameter inside a `FROM` subquery of `PREPARE` is typed before the
  subquery is described (`42883` today).
- [ ] A SQL function whose earlier statements are not queries or data changes
  type-checks its final statement as PostgreSQL does.

#### Rows, records and routine signatures

- [ ] A whole-row reference (`SELECT t FROM t`) has the table's row type
  (`record` today: `WHERE t = '(1,a)'` gives `42883`, not `0A000`).
- [ ] Row constructors cast to a view's row type; arrays of a table's row type
  (`ARRAY[ROW(1, 'a')::t]`, `0A000`).
- [ ] Table and view row types in routine signatures (`CREATE FUNCTION
  f(orders)`, `0A000`); PL/pgSQL variables of a view's row type and `%TYPE`
  of a view column (`0A000`).
- [ ] A subquery's `t.*` naming an enclosing query's item (`42601`/`42P01`
  today).
- [ ] A scalar function as a `FROM` item (`FROM t, nextval('s') x`, `0A000`).
- [ ] `INSERT INTO t VALUES (1) UNION SELECT ...` (`42601` today).
- [ ] A subquery reading `old` or `new` in `RETURNING` of a view write
  through an `INSTEAD OF` trigger or rule (`42P01`).

#### PL/pgSQL and rule actions

- [ ] PL/pgSQL `WITH ... INSERT/UPDATE/DELETE` statements (`0A000`), and
  PL/pgSQL writes with `WITH` queries whose nested `AFTER` trigger waits
  (`0A000`).
- [ ] Routine variables in a static write starting with `WITH`, a
  materialized `WITH` inside a derived table, and an ordinary `WITH` read by a
  data-modifying one (`42703`): pass the routine's row as
  `expand_plpgsql_with` does.
- [ ] A missing field of an anonymous row passed to a `record` parameter
  reports `record "r" has no field "a"` (`42P01` today).
- [ ] A `$n` beyond a routine's parameters in a statement that reads no rows
  is rejected at analysis.
- [ ] Block label versus row variable resolution compares block by block.
- [ ] A rule created inside a routine words a missing `OLD.` column as
  PostgreSQL does.
- [ ] A trigger fired inside a write can `FETCH` from a session cursor that
  calls a user routine per row (`0A000`).
- [ ] A `READ COMMITTED` recheck of a row joined through a `LATERAL` subquery
  or a `UNION ALL` branch that reads tables (`0A000`).
- [ ] Rule actions reading other relations, and `INSERT`s whose query has
  volatile output, stream their derived relation; an `INSERT ... SELECT` whose
  condition and output both call routines interleaves them per row.
- [ ] Joined `UPDATE ... FROM`/`DELETE ... USING` reaching foreign rows
  qualify them in the change loop; a routine reading a joined column is
  costed as PostgreSQL does.

#### Schema objects and catalogs

- [ ] Bind statistics expressions, trigger `WHEN` conditions and domain
  constraints to the objects they name, so renaming or moving a named routine
  or type, or renaming an enum value they cast to, works (`0A000` today).
- [ ] Renaming an enum value spelled in a SQL-standard body (`RETURN
  'a'::e`) keeps the call working (`22P02` today).
- [ ] Renaming a column a rule action names without `NEW.`/`OLD.` (`0A000`).
- [ ] Deparse subqueries, named-argument calls, `$n` and array constructors in
  stored expressions; non-`SELECT` statements of `BEGIN ATOMIC` bodies; policy
  expressions; rule actions with data-modifying `WITH` (`0A000`); `FROM`
  functions with column definition lists beyond `json[b]_to_record[set]`.
- [ ] `ALTER COLUMN ... TYPE` of columns that views, rules, trigger `WHEN`
  conditions, policies, SQL-standard bodies or stored writes read.
- [ ] Cascading drops of a view whose row type another column, attribute or
  domain uses (`0A000`).
- [ ] `DROP` of casts, operator classes and families, extensions, policies,
  triggers, rules, indexes and `DROP OWNED` describe dependents through the
  `pg_depend` walk; `pg_event_trigger_dropped_objects` orders every
  statement's objects by it; a TOAST table's and a dropped column's listing
  order.
- [ ] `DROP COLUMN` with inheritance children reports `cannot drop desired
  object(s)`; exclusion constraints join the shared `2BP01` report.
- [ ] `DROP ROLE`'s `DETAIL` lists `pg_shdepend` rows in physical order (needs
  a durable stamp per owner change).
- [ ] `pg_type` lists `information_schema`'s view row types (needs the pinned
  reference image).
- [ ] Shipped `pg_description` text of built-in conversions and collations;
  built-in `pg_conversion` rows.
- [ ] `pg_type`, `pg_operator` and `pg_opfamily` return built-in rows in a
  fresh cluster's physical order.
- [ ] `pg_rewrite` lists built-in views' rules (145 `_RETURN` rules,
  `pg_settings_u`, `pg_settings_n`) and has `ev_qual` and `ev_action`.
- [ ] `ALTER VIEW v DISABLE RULE r` fails with `42809` (`42601` today).
- [ ] `pg_trigger` lists the internal `PK_ConstraintTrigger_*` and
  `Unique_ConstraintTrigger_*` rows; `pg_constraint` lists the system
  catalogs' not-null constraints.
- [ ] Partition keys that are expressions or carry `COLLATE`; `HASH` keys of
  other types (`0A000`).
- [ ] A new partition's equivalent `UNIQUE` or `PRIMARY KEY` constraint is
  attached to the parent's plain unique index instead of building a copy.
- [ ] `CHECK ... NO INHERIT` (`0A000`); exclusion element operator class
  options (`0A000`).
- [ ] Heap storage parameters beyond `fillfactor` and `autovacuum_enabled`
  (`22023` today); `fillfactor` printed as written (`'070'`); view
  `reloptions` in the order set, with an explicit `security_invoker=false`.
- [ ] `ON CONFLICT DO NOTHING` without a target on a table with a deferrable
  constraint skips a row an earlier arbiter skips (`55000` today).
- [ ] A GiST exclusion violation names the partner PostgreSQL's index scan
  finds first.
- [ ] Index key calls: a key whose calls nest restarts once per build, not
  per row; `REINDEX INDEX`, writes and table rewrites stop repeating the
  other indexes' key notices; a partial unique index's predicate and key
  calls interleave as in PostgreSQL.
- [ ] `COPY FROM` into a view with an `INSTEAD OF INSERT` trigger (`0A000`);
  a refused `COPY FROM` into a view or sequence validates its options first.
- [ ] `COPY` to or from a server file or program, `COPY ... FREEZE`, and
  `ENCODING` other than UTF8 (`0A000`): implement or record as architecture
  boundaries in the compatibility guide.

#### Logical replication and postgres_fdw

- [ ] Per-table initial copy with PostgreSQL's `d`, `f` and `s` states (one
  bootstrap under one synchronization slot today).
- [ ] `ALTER SUBSCRIPTION ... REFRESH PUBLICATION` rejects a table published
  with different column lists in the statement (`0A000`), not in its copy.
- [ ] Publisher connection strings take libpq's defaults for port, user,
  dbname and sslmode (`22023` today). Host names are blocked
  ([B-351](BUGS.md)).
- [ ] postgres_fdw join pushdown (`foreign_join_ok`) and estimates for join
  inputs: `EXPLAIN` of a foreign table with other relations, under column
  aliases or in a derived table below a join fails with `0A000`, and the
  table is read whole.
- [ ] A SQL cursor or PL/pgSQL `FOR` loop over a foreign query fetches only
  the batches its reads need.
- [ ] A foreign scan's printed cost (zero today); a join over a `WITH` query
  reading a foreign table uses PostgreSQL's method.
- [ ] Remote grouping with `DISTINCT`, windows over groups, `LIMIT` with a
  remote order by grouping keys, postponed volatile expressions, and local
  `GROUP BY` under `LIMIT` follow PostgreSQL's costs.
- [ ] Ship InitPlans of `numeric`, character, time and interval types,
  correlated outer columns as parameters, and a custom plan's argument
  values.
- [ ] A correlated subquery fetches only the columns it reads.
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
[CONTRIBUTING.md](CONTRIBUTING.md) the verification gates. The roadmap is
finished when every condition in [the finish line](#the-finish-line) is
demonstrated.
