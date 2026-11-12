# How pos3ql works

pos3ql serves PostgreSQL SQL and protocol clients while saving durable state in
an S3-compatible object store. A configured object prefix contains one pos3ql
instance's durable state, including its SQL databases. One active process owns
the right to publish changes to that prefix. This is a single active writer
process, not a rule that every storage request must run in sequence. Ownership
is a conditional writer-fence object, not a time-based lease: a replacement
process fences the old one, whose next publication fails. There is no read-only
shared-snapshot protocol for other processes reading the same prefix.

**Example:** the same instance can contain `orders` and `billing` databases.
Their tables use the same startup memory budget and object-store namespace.
A backup or writer replacement concerns that namespace; creating a SQL database
does not create an independently operated storage service.

Startup may take exclusive recovery ownership before validating the remaining
journal. This prevents the previous writer from publishing while recovery runs;
a failed recovery can therefore leave that writer fenced out. The new process
accepts SQL writes only after journal replay and restored definitions validate.

## The request path

```mermaid
flowchart LR
    Client[PostgreSQL client] --> Server[Connection buffers and one SQL CPU worker]
    Server --> Rows[Transaction state and cached rows]
    Server --> Reads[Reserved object read slots]
    Reads --> Objects[Immutable data and index objects]
    Rows --> Log[Private journal records]
    Log --> Upload[Batch bytes and descriptor uploads]
    Upload --> Head[Conditional commit-head update]
    Head --> Commit[Install complete transaction and return success]
    Rows --> Save[Checkpoint and merge work]
    Save --> Objects
    Save --> Manifest[Conditional checkpoint-manifest update]
```

Connection input, parsed SQL, statement workspaces, transaction undo, locks,
object requests and output each have a fixed reservation made at startup. After
startup, ordinary runtime work cannot grow a heap, pool or sort allocation; the
isolated TLS transport uses its own separately budgeted pool. The memory plan
covers these reservations only, not total operating-system RSS, kernel socket
memory or the object-storage service. See
[memory and physical size](#memory-and-physical-size).

### Execution model

A **reactor** is the event loop that waits for socket readiness and deadlines.
Each turn it advances ready connections and a bounded amount of maintenance,
then registers newly opened storage sockets before sleeping, so cleanup keeps
moving on an idle server.

All SQL CPU work runs on **one SQL CPU worker**. Adding connections or
`query_workspace_slots` adds concurrent sessions and workspaces, not CPU
execution workers. Several CPU workers are future work:
[PLAN.md](../PLAN.md#run-sql-on-several-cpu-workers).

When a command needs an object that is not cached, it can **yield**: it keeps
its read requests, data owners and original command deadline, and the worker
runs another connection's cached work until the download arrives. A path that
cannot yield **blocks**: the read completes synchronously and occupies the
worker. Completion or failure releases unused read-ahead requests. Commit does
so as soon as its row images are in the journal, so an abandoned speculative
download cannot hold up the next checkpoint.

The rule for statements: a statement can yield until it performs its first
**effect** — something a restart could not undo or repeat. Effects are calling
a volatile function or a user routine body, issuing `nextval`/`setval`, and
output actually reaching the client. After the first effect, the remaining
reads of that statement block until the statement ends.

**The worker never waits for a client.** A result larger than the
connection's send buffer (`conn_send_buffer_bytes`) queues in the
connection's **overflow area**: rows lease space from the shared result spool
(`result_spool_bytes` / `result_spool_leases`), and a private reserve the
size of the send buffer always holds the closing `CommandComplete`,
`ErrorResponse` and `ReadyForQuery`. Socket writes are non-blocking for both
plaintext and TLS. While a client reads slowly, the statement still finishes,
and the connection then waits for the client to read its queued output before
it processes the next message (pipelined messages wait in order); other
connections keep running. A client that falls further behind than the spool
receives the rows already queued, then `54000` naming `result_spool_bytes`, and
its session stays usable; an implicit transaction rolls back and an explicit
one becomes failed. Notifications are held while a result is in progress and
delivered after it. Bytes leave during a statement only where no object-store
publication can follow: in local mode, and for a lone `SELECT`, `SHOW`,
`VALUES` or `TABLE` in object-store mode until it starts an effect; other
output waits for durable publication before any byte is written. A statement
attempt that has called a SQL-language routine may still replay, so its rows
stay queued until it finishes; if such a routine is first called after rows
already reached the client, the statement fails with `0A000` instead of
repeating them.

**Large results resume row by row.** Before a top-level `SELECT` or
`COPY ... TO STDOUT` runs (simple Query or extended `Execute`), the engine
decides whether it (or the query a COPY reads: its own, or `SELECT columns
FROM ONLY table` for a table) can become a **result stream**: one ordinary table, or
builtin set-returning functions with constant arguments (`generate_series`,
`generate_subscripts`, a one-array `unnest`, `regexp_matches`,
`regexp_split_to_table`, the `json`/`jsonb` `_each`, `_array_elements` and
`_object_keys` families; alone or in `ROWS FROM`, with or without `WITH
ORDINALITY`), or a **retained join** of up to eight such items or literal
`VALUES` lists (a nested loop, or a hash join built in `plan_build_bytes`
where the general executor would hash), optionally with an `ORDER BY` the
stream sorts itself (local mode), or a **retained grouping** of such a
source (`GROUP BY`, `HAVING`, aggregates and plain `DISTINCT`, below), or
`DISTINCT ON` the stream sorts (local mode), or a `UNION ALL` of such
queries without its own `ORDER BY` (below);
without grouping sets, other set operations, subqueries other than
uncorrelated scalar ones (a simple subquery in FROM is merged into its query
first, below), CTEs that run once, row locking
or row security, whose read position (one
record per resident row of each table plus the query text) fits
`result_stream_bytes`. A retained join uses inner, cross and `LEFT` joins,
or one `RIGHT` or `FULL` join of two items whose right item is a table or
`VALUES` list, whose `ON`/`USING` conditions are scalar, non-volatile and call no
SQL-language routine; a join whose inner table the general executor would
probe through an index for each outer row spools for `shape` instead. A
sorted stream's keys and filter must be non-volatile and call no
SQL-language routine; its output items either are too (they run before the
sort) or are volatile (they run after it, as PostgreSQL postpones volatile
output past a sort); with `LIMIT` or `OFFSET`, items that run before the sort
must be columns or constants. If such a statement's output first outgrows
the send buffer before any byte left and before any effect, that attempt is
discarded and the statement reopens from a retained read position in the
connection's `result_stream_bytes` allocation. It then delivers rows while the
client reads and yields when the client is behind (`Client`/`ClientWrite` in
`pg_stat_activity`), when a row waits for an object download, and when a row
waits for a lock. Delivered rows are final: a wait inside a row rolls back and
re-evaluates only that row, and an effect makes only the rest of that row read
synchronously. A delivered row's receipts (sequence values, routine results)
are released before the next row starts, so statement memory holds one row's
receipts however long the stream runs. Every other large result uses the spool above, decided before
any of its rows reach the client; `pos3ql_result_stream_spooled_total` counts
those by reason (`shape`, `capacity`, `effects`). A spooled result yields on
cold storage only until its first byte reaches the client, then reads
synchronously until it ends. Today that covers other joins, other groupings, sorted
results in object-store mode, subqueries, set operations other than `UNION ALL`, `VALUES` lists, other function sources,
row locking, row security, a result that started an effect before it
overflowed, and a `FETCH` from a scrollable cursor that saves its history. A buffered paged portal
sends nothing before its first page, so its reads yield throughout.

A paged portal whose statement can stream opens its stream before the
statement runs: its attempt would stop at its first row anyway, so a sort,
grouping or hash table is built once, in the stream, and its input reads
resume instead of restarting the statement on each cold read. In
object-store mode a join or grouping (including plain `DISTINCT`) opens its
stream before running under every protocol, for the same reason: its first
row reads a whole input. Such a stream opens however small its result, so
it counts in `pos3ql_result_streams_total`.

A statement that does not stream restarts from its start after each cold
read, finding the objects its earlier attempts read only in the block
caches. Once it has restarted more times than the caches and the object
read slots hold blocks (`block_cache_bytes`, `disk_cache_bytes` and
`object_store_get_slots`), its next attempt reads storage synchronously,
blocking the worker, instead of restarting again: its input may have
outgrown the caches, so each attempt could evict blocks the next one needs
and it would never finish. A result stream resumes from its read position
and never restarts this way.

A forward `FETCH n` or `FETCH ALL` (simple Query or unpaged `Execute`) from
a cursor that reads as it fetches and saves no history opens such a stream
over the cursor's own retained query before its first row: rows are evaluated
as the client reads them, none is saved in `cursor_bytes`, and the cursor's
position advances with each delivered row. A failure after delivered rows
marks the cursor failed, as PostgreSQL does. A cursor saves no history when it
is `NO SCROLL` or forward-only, and also when it is scrollable but unordered,
not locking, and free of volatile functions: such a cursor keeps only its
current fetch's rows, and backward motion beyond them rewinds its retained
input (table position, literal list or function arguments) to the first row
under the same snapshot and reads forward to the target, so memory stays
fixed however far the cursor has moved. Scrollable cursors that evaluate
revisited rows again, or that sort or lock, still save every visited row in
`cursor_bytes`. A cursor over a retained join reads its levels as it fetches
and is `NO SCROLL` unless declared `SCROLL`, as PostgreSQL's join plans
cannot scan backward; a `SCROLL` join cursor with a volatile output list or
filter saves its output rows, as PostgreSQL's Material node does, so a
revisited row returns its first values.

**Paged portals evaluate rows per `Execute`.** An `Execute` with a row limit
opens such a statement's stream before running it. Each `Execute`
evaluates only the rows it returns, then the portal suspends with
`PortalSuspended`, keeping its read position and snapshot while other
statements of the transaction run. The connection has `result_stream_slots`
stream allocations: a running stream uses one and each suspended portal keeps
one, so a client can page a second portal while the first is suspended. When
all are held, other large results spool for `capacity`, and another paged
portal buffers its whole result in `portal_result_bytes` (`54000` naming that
setting when full), as do paged portals of other shapes.
DDL on a table a suspended portal reads fails with `55006`, as in PostgreSQL. The
stream ends when its portal completes, is closed or rebound, or its
transaction ends.

**Example:** `SELECT id, payload FROM orders` sent by a client on a slow link
delivers rows as the client reads them; while it waits for the client or for a
cold block of `orders`, other connections run. `SELECT ... FROM orders ORDER
BY id` sorts first, so its output queues in the spool instead.

| Path | Cold storage read |
|---|---|
| Queries before their first effect, including reads inside explicit transactions | Yields |
| Journal uploads and the commit's wait for its durable receipt | Yields |
| Maintenance: checkpoint capture, merges, index builds, journal and saved-file cleanup | Yields, using a separate maintenance read owner and fixed clients |
| A statement after its first effect (volatile function, routine body, sequence change, first output byte reaching the client) | Blocks until the statement ends |
| Output queued for a slow client | Never blocks; the connection waits for the client between messages |
| Result streams (see above), including `COPY ... TO STDOUT` and a streamed `FETCH`: each row before its first effect | Yields; delivered rows are kept |
| Writing statements: `INSERT`, `UPDATE`, `DELETE`, `MERGE`, data-modifying `WITH`, DDL, `COPY FROM` | Blocks for the whole statement |
| Top-level `CALL` and `DO` bodies: queries before the statement's first effect | Yields; the body resumes at its waiting statement |
| Writes and statements after the first effect inside a `CALL` or `DO` body | Blocks |
| PL/pgSQL `OPEN`, `FETCH` and cursor `FOR` loops reading cold rows (a lock wait parks) | Blocks |
| Commit processing, deferred constraint triggers, `SET CONSTRAINTS`, `PREPARE TRANSACTION`, `COMMIT PREPARED`/`ROLLBACK PREPARED` | Blocks |
| `COPY FROM` rows and completion; a `COPY TO` that cannot stream once its first byte reaches the client; logical-replication apply and initial copy | Blocks |
| Large-object fast-path calls, login event triggers, manual `CHECKPOINT` | Blocks |

Lock waits are separate: a command waiting for a row, table or advisory lock
parks with its saved state, including inside procedural code, and the worker
runs other connections. Converting the blocking paths is tracked in
[PLAN.md](../PLAN.md#pause-and-resume-every-query-path).

**Example:** while one connection waits for a cold `orders` column, another
connection's cached lookup and a third connection's commit can finish. If the
first connection's statement had already called `nextval('order_ids')`, its
remaining reads would instead block the worker.

### Retained plan tree

A result stream, a retained cursor and a top-level `SELECT` run in place
read their rows through a **plan tree**: a chain of nodes that each produce
one row at a time and keep their position between rows. The chain is a leaf
(a scan of one table position, literal list, generator calls or FROM-less
row; or a join whose levels are such scans), an optional filter, an optional
sort or grouping, the projection and the limit. Each node is a fixed record
inside the reader, which lives in the stream's `result_stream_bytes`
allocation or the cursor's slot; sorts, groupings and hash joins keep their
rows in `plan_build_bytes` leases. Every node implements the same steps:
advance to the next row (or report the end), expose the current row, release
it once the driver is done, rewind for a scrollable cursor. A wait inside any
node returns to the driver with every node's state kept, and the same step
resumes. The filter, projection and limit at the top run in the statement
driver itself, so a routine call there hands off on its row; a filter under a
sort or grouping runs as that node reads its input
(`src/sql/query/retained/tree.rs`).

**Aggregate levels.** Before a query is planned, each aggregate call is
given to the query PostgreSQL assigns it: the innermost one whose columns its
arguments read. `SELECT (SELECT max(t.id) FROM u) FROM t` aggregates `t` and
returns one row. Analysis rewrites such a call inside a subquery as an outer
aggregate; the owning query folds it with its own aggregates and evaluates
that subquery once per group, whose row supplies the value. The same pass
reports PostgreSQL's placement errors (an aggregate in `WHERE`, `JOIN ... ON`,
`GROUP BY`, `FILTER`, `LIMIT`, a function in `FROM`, or nested in another of
its level) before any row is read (`src/sql/query/aggregate_levels.rs`).

**Subqueries in FROM.** Before a query is planned, each simple subquery in its
FROM clause is merged into it, as PostgreSQL's `pull_up_subqueries` does: the
subquery's FROM items join the outer query in its place, its `WHERE` joins the
outer restrictions, and every reference to one of its output columns
becomes that column's expression. So `SELECT * FROM (SELECT 10/x AS q, x FROM
t) s WHERE x <> 0` filters before it divides, and an output column nothing
reads is never computed. A subquery is merged only when that is exact: no
grouping, aggregates, window functions, `DISTINCT`, `ORDER BY`, `LIMIT`,
locking, set-returning or volatile output, security-barrier view, or name the
merge could rebind. A table among its FROM items whose name another item of
the query already has (a `NOT MATERIALIZED` CTE read twice, a self-join
through two derived tables) takes a name no SQL text can spell, longer than
the 63 bytes kept of an identifier, as PostgreSQL tells such items apart by
position. Under an outer join that can null its rows it merges only
without a `WHERE` (which would otherwise run per joined pair) and when each
output it substitutes is null whenever its columns are (PostgreSQL substitutes
those without a placeholder). A view merges when the caller holds `SELECT` on all of it.
`USING` and `NATURAL` joins of the outer query are first spelled out as `ON`
joins. A subquery that stays in place receives each outer `WHERE` conjunct
that reads only its columns, under the view-filter safety rules (no bounds,
volatile, window or `DISTINCT ON` hazards); and every query moves `HAVING`
conjuncts without aggregates, volatile calls or subqueries into `WHERE`, so
they filter rows before aggregation. This happens in the general executor and
in stream admission alike, so a merged derived table streams like the query it
became (`src/sql/query/pullup.rs`).

**CTEs and views in streams.** Stream admission expands a query's `WITH`
list and views as the general executor does, then merges the result as
above. A CTE is inlined, as PostgreSQL's planner inlines it, when nothing
forces it to run once: it is not recursive or data-modifying, not
`MATERIALIZED`, calls nothing volatile, and is read once (or is `NOT
MATERIALIZED`). A CTE nothing reads never runs. When any CTE, at any depth
or inside a view, must run once, the statement spools for `shape`: the
general executor runs that CTE to its end before the first row. A view
expands under its owner's privileges and is locked as the general executor
locks it; it merges only when the caller holds `SELECT` on the whole view,
and otherwise (column privileges only, `security_barrier`) spools. So
`WITH recent AS (SELECT * FROM orders WHERE id > 100) SELECT * FROM recent`
streams as the scan of `orders` it becomes, while the same query with `AS
MATERIALIZED` spools (`src/sql/query/cte.rs`, `expand_stream_ctes`).

**Built: uncorrelated subqueries.** A stream, or a top-level `SELECT` run in
place, keeps scalar, `EXISTS`, `ARRAY`, row and `IN`/`ANY`/`ALL` subqueries
that read no column of its query and call no volatile or user-defined
function (one over a set-returning function whose arguments read no column,
such as `FROM generate_series(1, 3)`, reads none). Each runs when an expression first reads it, as PostgreSQL's
initPlans and hashed subplans do (in the general executor too), and keeps its
value for the rest of the statement in the reader's allocation
(`result_stream_bytes`) or the statement arena's persistent tail, so `SELECT
(SELECT 1/x FROM z) FROM empty_table` and `SELECT id FROM t WHERE id > 5 AND
id IN (SELECT 1/x FROM z)` return no rows. A list keeps its members as encoded
rows there, or in object-store mode as the external run it was written to,
with its column type, collations and whether a member was NULL. The operand's
type is checked against the list's declared column type while the query is
analysed, so `'zz' IN (SELECT int_column ...)` fails even when no row reaches
it. A `WHERE` conjunct that reads no column of its query runs once before
the scan, as PostgreSQL's one-time filter: `SELECT id, payload FROM orders
WHERE id > (SELECT max(id) - 1000 FROM orders)` streams, and a false one
returns no rows without reading any. `UPDATE`, `DELETE`, `INSERT ... VALUES`
and `ON CONFLICT DO UPDATE` plan their subqueries the same way
(`DmlSubqueries` in `src/sql/query/subquery.rs`): an uncorrelated one runs
when first read, a correlated one (reading the target, `FROM`/`USING` or
`excluded` row) for each row that reads it, and their one-time filters run
before the target scan. Every one reads as of the statement's command, so a
subquery first run after the statement wrote rows does not see them. A
FROM-less `SELECT` (an `INSERT ... SELECT` source included) also runs its
subqueries when first read. A wait inside a subquery runs it again. A sorted or grouped stream keeps
them too: they count as stable, as PostgreSQL's initPlans are, so a sort
evaluates them before it orders the rows. Correlated subqueries, and
subqueries in a `UNION ALL` query or in a declared cursor, still spool or
save their rows. In the general executor
a correlated subquery reads the enclosing row from its derived tables, set
operation branches, `LIMIT`, `OFFSET` and `JOIN ... ON` conditions, as
PostgreSQL's outer references do; a correlated subquery in a join condition
runs for each pair of rows it compares. A correlated scalar, `ARRAY`,
single-column `IN`/`ANY`/`ALL` or `EXISTS` subquery binds whether the
row-source executor runs it, its result type, collations and aggregate calls
once per statement, before any outer row, and copies the bound aggregate
states for each outer row (`bind_correlated_subqueries` in
`src/sql/query/subquery.rs`). An uncorrelated subquery nested in a
correlated one, a `LATERAL` FROM subquery or a FROM function's arguments,
at any depth and in set operation branches, takes a slot the outermost statement reserves before its
scan (`InitPlans::nested`), so it runs once for the statement as
PostgreSQL's initPlan does: `SELECT (SELECT x + (SELECT nextval('s')) FROM z
WHERE x <= t.id) FROM t` reads one sequence value, and 100000 outer rows do
not each keep a copy of its value. A slot is keyed by the sublink's kind and
its `SELECT` (`Sublink` in `src/sql/eval/mod.rs`), not by the expression
node, because rewriting a query for an outer row (merging a FROM subquery,
pushing a restriction into one) copies the nodes around substituted columns
but never an uncorrelated subquery's own `SELECT`. Every way a nested query
prepares its subqueries consults the slots carried by its outer row lookup. A grouped query whose aggregates keep only
running values (`count`, `sum`, `avg`, `min`, `max`, the boolean, bitwise and
statistical aggregates, without `DISTINCT` or `ORDER BY`) copies a
variable-width running value such as a text maximum or numeric sum into a
reused slot after each row (`HeldValues` in `src/sql/query/aggregate.rs`), so
the scan reclaims the row's scratch; a derived table kept in local mode
stores only its encoded rows.

Every other shape runs in the general executor, which materializes in the
statement arena and cannot pause after its first byte. New node kinds are the
planned route for subqueries, set operations and sorts of results in
object-store mode, for removing the per-call restart of a top-level `SELECT`
in those shapes (each routine call then hands off on its row, as single
sources and retained joins already do), and for removing synchronous cold
reads from spooled results after their first byte.

**Built: nested-loop joins.** A retained join keeps one level per FROM item
in the general executor's execution order (FROM order, or its cost order for
a cross join). Each level owns its source position, its current row in the
reader's allocation, and for a `LEFT JOIN` whether the row above found a
partner; an inner level rewinds its source for each row of the level above,
as PostgreSQL's nested loop rescans its inner plan. Rows nest in the
allocation like a stack, so finishing a row releases exactly its bytes. The
`ON` condition and the error-safe `WHERE` conjuncts whose tables are bound
run at each level, as the general nested loop prunes; the full `WHERE` and
the output list run in the statement driver, so routine calls hand off on
their row. A storage wait at any level returns to the driver with every
level's position kept, and the same row is read again on resume. Admission
reuses the general planner's choice, so streaming never turns a hashed or
indexed join into a full nested loop: a two-table equality join the general
executor would hash is read as a hash join that builds the same side, and a
join it would probe through an index per outer row is not retained.

**Built: `RIGHT` and `FULL` joins.** As in the general executor, a `RIGHT`
or `FULL` join of two items is a nested loop over the left item that keeps
one flag per row of the right item, in a `plan_build_bytes` lease that
grows as the first left row's pass reads the right item. Each right row that
passes `ON` sets its flag; a `FULL JOIN` also returns each unmatched left
row NULL-extended. After the last left row, the right item is read once
more and each row whose flag is clear is returned with the left columns
NULL, in the right item's order. A `FULL JOIN` whose `ON` condition, once
constant conjuncts fold away, has no equality between an expression of the
left tables and one of the right table fails with `0A000`, as PostgreSQL
plans it only as a merge or hash join. A scrollable cursor rewinds both
items and clears the flags.

**Built: hash joins.** The build level reads its whole table once, at the
join's first row (a declared cursor's at `DECLARE`), into three `plan_build_bytes` leases owned by the level:
bucket heads sized from the planner's row estimate, one entry per row, and
the rows' columns in projected encoding. A wait during the build keeps the
rows inserted so far and the table position. Each outer row hashes its keys
and walks its bucket; equal keys are compared, then the `ON` condition runs,
and a `LEFT JOIN` returns a NULL-extended row when nothing matched. A cursor
moving back rereads only its outer table: the build stays. A build that does
not fit the free `plan_build_bytes` runs the statement the general executor's
way instead (below), which builds the same table in statement memory
(`sql_arena_bytes`).

**Built: grouping.** A stream or a top-level `SELECT` run in place (not a
declared cursor) groups a single source or retained join itself when its
keys, `WHERE` and aggregate inputs are non-volatile and call no SQL-language
routine, its custom aggregates' support functions are builtins (in
object-store mode, also not combined in parts), and its `ORDER BY`, if any,
is a prefix of its keys ascending with NULLs last; without grouping sets or
`DISTINCT ON`. A plain `SELECT DISTINCT` without grouping or aggregates
groups by its output list the same way: the general executor also returns
it sorted by that list, and each distinct value keeps its first row's
spelling (`1` before `1.00`), as PostgreSQL's hashed `DISTINCT` does; the
general executor's sort is now stable for that reason. At its first row it reads its whole input and filters it; a
storage wait keeps what was saved and the input position. How it saves its
input is decided before any row, from the aggregates' kinds and argument
types:

- **Per group** when every aggregate's transition state has a fixed size or
  holds one value: `count`, `sum`/`avg` over integers, `money`, floats and
  `interval`, `min`/`max` of any type, `bool_and`/`bool_or`/`every`, the
  bitwise aggregates over integers, variance and standard deviation over
  floats, and the two-argument statistical aggregates, without `DISTINCT` or
  an aggregate `ORDER BY`. Each group keeps its key and a few bytes per
  aggregate in `plan_build_bytes`, found through a balanced tree ordered by
  key; a `min`/`max` of a type passed by reference (text, numeric, arrays)
  keeps its current value there too, rewritten in place when the new one
  fits. Each input row folds into its group as it is read, and a group's
  results are computed as its row is returned: `SELECT k, f(count(*)) FROM t
  GROUP BY k` holds one record per distinct `k`, however many rows `t` has.
- **Per input row** otherwise (numeric sums, collecting, ordered-set and
  custom aggregates, `DISTINCT` or `ORDER BY` inside an aggregate): it saves
  each row's keys and the input columns its aggregates read, orders them by
  key (equal keys keep input order), folds each group over its rows and
  saves its keys and results in two more leases.

Either way a group folds its rows in input order with the general
executor's aggregate states, each initialized once per aggregate and copied
for every group (the general executor now does the same). Transition states never cross a wait inside a
row: only reading input waits, and folding calls no routine body. Groups
come out in the general executor's order (keys ascending, NULLs last), each
reporting its first input row's key as PostgreSQL's hashed grouping does
(the general executor now does too). `HAVING` and the output list run per
group as its row is returned, so a routine call there hands off on its row.
An error in an aggregate's input is reported before the first row; one in
an aggregate's result (an overflowing `sum`), `HAVING` or the output list
follows the groups already returned, as PostgreSQL's executor returns them
(per input row, the results are computed before the first row). A grouping that does not fit
`plan_build_bytes` runs in the general executor, as a sort does. A top-level
`SELECT` is run in place only when its output list, `WHERE` or `HAVING` may
hand a call off to a routine; builtin aggregate calls alone, as in `SELECT
count(*) FROM t`, stay in the general executor.

**Nodes.** A node never repeats an effect: row receipts stay with the row
being built and are released when it is delivered.

| Node | State it retains | Memory owner |
|---|---|---|
| Scan (built: table, literal list, generator calls, FROM-less row) | The read position and current row | Stream or cursor allocation |
| Filter, projection, `LIMIT`/`OFFSET` (built) | The condition, items, and remaining counts | Node record |
| Nested-loop join (built: inner, cross and `LEFT`; `RIGHT` and `FULL` of two items) | Each level's row bytes, position and matched flag; a flag per right row for `RIGHT`/`FULL` | The stream or cursor allocation, the right-row flags in `plan_build_bytes`; the inner source rewinds as scrollable cursors do |
| Hash join (built: two tables, inner, cross and `LEFT`) | Build-side rows, entries and bucket heads | `plan_build_bytes` / `plan_build_leases`; exhaustion is `54000` naming them |
| Grouping (built: `GROUP BY`, `HAVING`, aggregates) | Each group's key, states and held values; or every input row's keys and aggregated columns, then each group's keys and results | `plan_build_bytes` |
| `DISTINCT` (built: plain, as a grouping by the output list) | Each distinct output row | `plan_build_bytes` |
| Sort (built: local mode, also for `DISTINCT ON`) | Sort keys, saved output and input columns of every row | `plan_build_bytes`; object-store mode: the external sorter's runs on the block grid (not built) |
| Set operations (built: `UNION ALL` in streams) | `UNION ALL`: each query's own plan and the current query; others: sorted runs of both inputs with counts | Stream allocation; others: external runs |
| Scalar, `EXISTS`, `ARRAY` and row subquery (built: uncorrelated, unsorted and ungrouped, run when first read) | Each value; correlated: a per-outer-row child tree, rewound per row | Stream allocation |
| `IN`/`ANY`/`ALL` subquery (built: uncorrelated, run when first read, kept as encoded rows or an external run) | A cached sorted run when uncorrelated, or a per-outer-row child tree | External run |
| Derived table, CTE read once | Child tree in place | Its nodes |

**Choosing a stream before execution.** Eligibility is decided from the
query's shape before it runs: a statement either opens as a tree (all its
nodes supported) or takes today's path, and the stream's eligibility and its
reader share one admission function, so they cannot disagree. Promotion never
makes a result fail that would have spooled. A stream keeps its opening
messages (RowDescription, or CopyOutResponse and a COPY header) in a 16 KiB
part of its `result_stream_bytes` until its first row is prepared, which is
when its sort or hash table is complete. If opening the stream or building
its table fails for capacity (`54000`: `result_stream_bytes`, the opening
messages, or `plan_build_bytes`), nothing of it has been queued and no row or
effect exists, so the connection releases the stream and runs the statement
again with its output spooled, counted in
`pos3ql_result_stream_spooled_total{reason="capacity"}`. This holds for a
simple Query, an unpaged or paged Execute and `COPY ... TO STDOUT`; a re-run
that waits for storage spools again rather than promoting. Any other error
during the build follows the opening messages, as PostgreSQL sends them first.
A top-level `SELECT` that a retained reader runs in place (one whose routine
calls hand off on their row) holds its RowDescription the same way and runs
in the general executor when its build does not fit. A declared cursor builds
its hash table at `DECLARE`; one that does not fit saves its rows in
`cursor_bytes` instead, so a streamed `FETCH` never builds.

**Built: `UNION ALL`.** A stream (not a declared cursor) reads a `UNION
ALL` as PostgreSQL's Append node does: each query has its own plan in the
stream's allocation and is read to its end before the next one starts. A
query's filter, bounds and output run as its rows are read, below the
statement driver, so every query's expressions must be non-volatile and call
no routine body; its values are coerced to the combined column types (as the
general executor coerces them) before the row goes up. The physical tables
of all queries together are limited to eight, as a retained join's are. A
`LIMIT` or `OFFSET` on the whole `UNION ALL` applies to the combined rows.
Every protocol opens it before running (a simple Query, an unpaged or paged
Execute and `COPY`), since the general executor's attempt would materialize
every query before its first row could overflow; a result that small still
streams, and one without a free stream slot spools for `capacity`.

**Built: sorts in local mode.** At its first row a sort reads its whole input
(yielding on storage waits with the rows saved so far kept), filters it, and
saves each passing row in projected encoding in two `plan_build_bytes`
leases: the sort keys, the output items that run before the sort, and, when a
volatile item runs after it, the row's input columns. It orders the rows with
an in-place heap sort (equal keys keep input order) and then returns them one
by one; a volatile item runs as its row is returned, so `nextval` numbers
rows in output order. `DISTINCT ON` sorts by `ORDER BY`, then by its
expressions missing from it, ascending, and returns only the first row of
each run of equal expressions; as in the general executor, its output items
all run before the sort. The choice depends only on the storage mode, never on
the input size: in object-store mode the general executor sorts in external
runs on the block grid, and such a result spools. Streaming those runs needs
a run reader per stream, sized at startup with `result_stream_slots`, since
the external sorter today is a shared pool of eight statement-scoped
producers.

**Cursors.** A scrollable cursor over a tree moves back by rewinding: scans,
literal lists and generator calls rewind as now, sort nodes reread their
finished run, and joins rewind their outer source. A tree containing a
volatile function under a node that cannot evaluate rows again saves its
outputs, as scrollable cursors over such shapes do now.

### How saved state is owned

A paused command must resume exactly where it stopped, without repeating
effects. The general rule: **everything needed to resume moves out of
temporary statement memory into a startup-reserved owner before the command
pauses**, and every completed effect leaves a **receipt** that resumption
replays instead of running the effect again.

| Saved state | Owner |
|---|---|
| Procedural positions, lexical frames, source images, resolved local declarations, loop scopes, named-output bindings, caught error text | `execution_control_bytes` / `execution_control_leases` |
| Saved local values, trigger `OLD`/`NEW` rows and arguments, transition tables, loop inputs, set-returning results, `EXECUTE ... INTO` results, finished expression values of a waiting statement | `row_work_bytes` / `row_work_leases` |
| Completed function results, argument values, selected functions and casts, sequence and advisory-lock receipts | The statement arena's retained region (the invocation log), within existing statement capacity |
| Checked literal inputs saved with routines and stored definitions | `table_metadata_bytes` / `table_metadata_leases`, with `routine_input_slots` control records |
| Execution schemas | `definition_image_slots` |
| Result output queued for a slow client | Rows: `result_spool_bytes` / `result_spool_leases`, released as soon as the client drains them. Closing messages: a private reserve of `conn_send_buffer_bytes` per connection |
| A result stream's query text, parameters, row positions and saved-file readers | `result_stream_bytes` per stream, `result_stream_slots` streams per connection; released at completion, error, cancel, timeout or disconnect, and for a paged portal when the portal is closed or rebound or its transaction ends. Not visible in `pg_cursors` and not counted in `max_cursors`. A streamed `FETCH` keeps its rows' state in its cursor's own allocations |

Further rules that apply to every saved owner:

- **Environment.** A paused call saves its search path, settings, effective
  user, statement and transaction timestamps and original deadline, and
  reinstalls them on resume. Another connection cannot lend it settings or
  renew its timer. A failed reattachment restores the caller's environment.
- **Identity checks.** Saved state records its connection, database,
  transaction, command and arena. Resuming under a different owner fails with
  `40001` or an internal error before changing any state, and leaves the saved
  state intact.
- **Receipts.** For example, a function that writes an audit row, issues an
  order number, then waits for an advisory lock resumes with the same local
  values and writes that audit row once. `PERFORM write_audit('one'),
  wait_for_lock(), write_audit('two')` inserts each row once even if the lock
  waits repeatedly; a second lock call in the same statement reuses the first
  call's receipt instead of taking another hold.
- **Unsupported restarts fail loudly.** A path that cannot save its progress
  returns `0A000` and rolls back rather than repeat a write — for example
  `SQL row expression requires retained write progress`, or an unsaved volatile
  value such as `random()`. Contexts with no resumable executor reject advisory
  unlocks with `0A000`.
- **Typed wait markers.** Internal waits use a typed marker, not a SQLSTATE.
  An application raising `PZ001` returns or catches it normally; it cannot park
  a connection.
- **Exhaustion.** A full pool returns `54000` naming its settings. Growing a
  saved buffer reserves its replacement first, so failure keeps the previous
  value readable.

A top-level `CALL` or `DO` keeps its waiting PL/pgSQL body on the
transaction: lexical position, local values, settings, timer and journal
position, in the same pools as a saved function. A nested `CALL` keeps the
callee's body in its caller's saved frame. Each body statement is its own
rollback boundary: a write that waits for a row lock rolls back only itself
and runs again on resume, so the body can wait after its own `COMMIT` or
`ROLLBACK`, which start new transactions without ending the statement or its
effect journal. Ending the statement by error, timeout, cancel or disconnect
releases the saved body.

Write completion follows PostgreSQL's
[write executor](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/executor/nodeModifyTable.c):
drain row events, run AFTER statement triggers in order (MERGE runs them in
DELETE, UPDATE, INSERT order; `INSERT ... ON CONFLICT DO UPDATE` runs UPDATE
before INSERT), then return one completion. Direct table writes without rewrite
rules hand that completion to a retained owner, so a waiting AFTER trigger
resumes without redoing the insert, update or delete, and earlier `RETURNING`
rows are kept. Nested INSERT, UPDATE and DELETE inside a trigger hand their
AFTER work to the calling trigger. BEFORE and INSTEAD OF triggers, modifying
CTEs, rewrite rules, `COPY` and replication apply do not yet have this owner.

With `track_functions = 'all'`, an active function timer owns one of 1,024
entries on the SQL execution thread. A paused call releases its entry and
re-admits it on resume, so another client's calls are not counted as its
children. Exhaustion returns `54000` naming the function timing stack.

### Statement preparation

Function calls bind their selected function, argument mapping and declared
types before running arguments, using PostgreSQL 18.6 declarations through
static indexes rather than a growing candidate list. Defaults keep the object
identities captured at creation, so a renamed sequence cannot be replaced by
another sequence with its old name. Planner safety facts cover all 3,397
procedures in the pinned PostgreSQL catalog, including internal ones that are
not callable from SQL;
[source hashes](https://github.com/e6qu/pos3ql/blob/d79ba0232874d6dfbd7a9f7a744af9a9a12f3318/tests/external/reference/2026-11-03-input-plans/planner-procedure-properties.json)
record their origin.

Some literals are checked when SQL is prepared, such as
`'{approved}'::checked_status[]`, where `checked_status` is a domain (a type
with extra constraints). The checked value belongs to the SQL source that
analyzed it:

| Source | When inputs are checked and how long they are kept |
|---|---|
| Simple command | At admission; a new command checks again |
| SQL `PREPARE` or protocol Parse | Once; kept with the statement and its portals |
| SQL function or procedure body | `CREATE` validates; the first runtime call in a backend checks again and the backend keeps the result |
| SQL function considered for inlining | By the caller's planner; kept with the caller's plan if inlined |
| Static PL/pgSQL statement | When first reached; kept by the backend for that definition, input types and collation |
| Dynamic `EXECUTE` | Every execution |
| View, rule, default or `BEGIN ATOMIC` body | With the definition; kept in its immutable metadata image and restored at recovery without re-checking |

`DISCARD PLANS` makes later admissions prepare again; running calls, portals
and cursors keep their checked images. `DISCARD ALL` also releases cached
routine-input plans. Neither re-evaluates inputs saved in schema definitions.
Native regressions in `src/sql/tests/type_input_binding.rs` and
`src/sql/tests/type_input_positions.rs` retain the PostgreSQL 18.6 reference
SQL digests for these distinctions.

Parse, Bind, Describe and `COPY` each restore their own connection's user,
search path, timezone and command timestamps. The original command deadline is
not renewed between protocol messages; an expired Sync still completes error
recovery with ReadyForQuery. Once durable publication starts, a timeout cannot
cancel the required commit receipt.

Leaving a routine configured with `SET` restores only the settings named in that
clause; an explicit session-wide `SET` inside it survives. A connection-wide
64-bit setting counter never wraps; exhausting it returns `54000` with
`session setting assignment counter exhausted`. These rules follow PostgreSQL's
[SET contract](https://www.postgresql.org/docs/18/sql-set.html).

PostgreSQL reference captures for procedural execution:
[nested results](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-nested-callback-results/README.md),
[saved output scopes](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-saved-output-scopes/README.md),
[name bindings](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-procedural-name-bindings/README.md),
[lexical scopes](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-procedural-lexical-scopes/README.md),
[parameter scopes](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-procedural-parameter-scopes/README.md)
and [caught diagnostics](https://github.com/e6qu/pos3ql/blob/main/tests/external/reference/2026-11-06-caught-diagnostics/README.md).
The [compatibility guide](postgresql-18-compatibility.md) owns the supported
SQL and procedural surface.

### Commit publication

A commit stages private journal records, uploads them, then conditionally
advances the commit head before the transaction becomes visible and the client
sees success; [writes and the success response](#writes-and-the-success-response)
gives the steps. A storage wait during publication resumes that publication; it
never executes the command again. Standalone utilities with an immediate commit
boundary, such as `CREATE DATABASE`, finish durable publication before their
Execute response.

## Reads and storage layout

The durable layout is log structured: new transactions and saved generations
append immutable objects. Small mutable roots identify the committed log and
checkpoint inventory. An object with a content-derived name is never edited in
place. Local RAM and disk accelerate access and can be rebuilt from durable
state in object-store mode.

| Mechanism | What it saves | Example and limit |
|---|---|---|
| Full-slice column groups, called PAX | Downloads for columns that are not needed | Reading `id, status` can skip a large description column; predicates, reconstruction and exact checks can still require more columns |
| Packed, compressed row deltas | Descriptor and column-object overhead for small changes | Updating a few orders writes small row groups rather than a full wide-table slice |
| Eight-byte column extent alignment | Gives encoded column values aligned offsets within packed objects | This is a layout guarantee, not a CPU throughput or sector-size claim |
| Packed objects and verified byte ranges | Request overhead and unnecessary transferred bytes | Several independently verifiable extents can share an object; a ranged GET fetches the selected extent |
| Sparse row indexes and Bloom filters | Reading generations or groups that cannot contain a row | An absent order number can avoid a data read; several generations can still need checking |
| Durable value indexes | Searching unrelated rows or regions | Btree/hash, geometric bounds, interval bounds and GIN postings narrow candidates; exact SQL and visibility checks still determine results |
| Reuse of unchanged immutable groups | Rewriting already saved data | Checkpoint/merge work can reference unchanged PAX groups; changed groups still need output |
| Memory/disk caches and retained reader buffers | Repeated downloads and repeated decoding after a wait | A resumed query retains completed columns and navigation even if the shared cache evicted them |
| Bounded batches of object requests | Serial network round trips | Missing selected columns and independent physical writes can overlap within configured request slots |

These mechanisms are implemented; their production benefit has not been
quantified. PAX is a column-group layout within saved row generations. It does
not imply a separate user-selectable columnar table access method, vectorized
SQL execution, multi-core queries or Parquet compatibility.

An immutable sorted row file, or SST, orders versions by row identity and
commit position. Newer versions are considered before older versions for the
same row. A reader combines applicable saved generations with its resident
changes and deletion markers. Large rows can require an overflow chain;
reading one row is therefore not a fixed number of GET requests.

Eligible two-source equality joins use a bounded hash build over tables,
catalogs, derived results and external sort runs. Up to eight equality keys,
NULL keys, duplicate matches, extra `ON` conditions and `LEFT JOIN` share that
path. A build estimated beyond its fixed 32,768-entry ceiling, a lateral or
partitioned source, or incompatible key types selects a nested-loop plan before
execution; `EXPLAIN` reports the same choice.

See [index navigation](index-navigation.md) for searchable summaries and exact
checks, and [durable formats](durable-format.md) for encodings and accepted
readers. PostgreSQL's catalog alignment and heap properties describe the SQL
interface; they do not prescribe pos3ql's object layout.

## Writes and the success response

1. SQL checks permissions, constraints and locks and prepares private row and
   catalog changes. Rollback records and journal staging must fit their pools.
2. Before durability, admission reserves replacement table/type definitions,
   index-binding roots and row keys. It retains the completed command while
   required locks or storage are unavailable.
3. A commit captures immutable journal bytes and a descriptor. Independent
   uploads can overlap, and several transactions can share the publication.
4. A conditional update advances the commit head only after the required
   uploads succeed. It preserves the committed predecessor and writer identity.
5. The complete transaction installs in memory, its visible position advances,
   locks release, and the server can send success.

**Example:** updating an order and its invoice in one transaction cannot expose
one changed value and one unchanged value during installation. An older report
can retain its earlier values. A new command waits through the installation
boundary rather than observing half a transaction.

Admission is cancellable before durable publication. A wait preserves
completed defaults, triggers and buffered results; resuming continues the
commit instead of executing the command again. Share relation locks protect
affected tables: ordinary SELECTs and unrelated work can proceed, while
conflicting writes wait. Publication consumes reserved owners rather than
allocating or rebuilding metadata after durability.

After a restart, saved prepared transactions reconstruct their private row and
catalog changes from the journal, including types, indexes and stored queries,
bound in their own database against their own private definitions. Other
transactions still see the committed definitions. `COMMIT PREPARED` completes
admission before making its decision durable, then installs the reserved
changes through normal publication; `ROLLBACK PREPARED` releases them with the
same captured identities and ordinary undo records.

The commit head is ordered because confirmed transactions need one recoverable
history. This does not require serial uploads. More upload slots do not remove
root-update latency or conflicts between transactions updating the same row.

With `object_store = off`, success instead requires local journal durability.
That mode cannot recover lost local-only data from an object store.

No success response crosses the durability boundary. If durable publication
fails, buffered success responses waiting on it are replaced by that error; a
client whose commit was staged instead receives fatal `08007` `commit outcome is unknown;
reconnect before checking it` and its connection closes. With object storage
on, a lost success reply is an uncertain client outcome, not
permission to repeat an external payment blindly.

## Maintenance and scheduling

The server advances checkpointing, file merges, history cleanup, heap relocation
and eligible deletion in bounded turns, between client messages and while idle.
No operating-system cron entry is needed for this internal work. Socket
readiness resumes waiting network requests; owner contention and failed
maintenance retry every second. A quota of 64 visited row slots per turn is a
work bound, not a latency guarantee.

- **Checkpoint:** writes captured committed versions and publishes a complete
  manifest, the inventory of saved objects and definitions. Later commits stay
  in the journal. Finishing an older checkpoint cannot clear newer writes or
  ANALYZE results. Index output is saved only when both its catalog identity
  and its row-decoding context are committed.
- **Merge:** combines saved generations, keeping versions needed by readers.
- **Deletion:** respects current roots, retained readers and backups.
- **History cleanup and relocation:** visit the ordinary shared row pool once
  per pass and the large-object pool separately; a typed cursor reports current,
  skipped-stale or complete positions, and generation checks prevent a reused
  cell from being treated as the earlier row.

Maintenance waits do not block foreground work. Checkpoint row capture, row
output, index input scans, sort-run merges and index output share one
maintenance read owner, reserved in the fixed read pool alongside connections
and subscription workers, with private startup-reserved row and scan buffers.
Journal cleanup and saved-file cleanup each use their own fixed metadata
client, backup list and deletion queue, and keep their listing positions across
waits. Saved-file cleanup validates the complete keep-set before deleting;
durable file writers pause while that decision drains, while foreground RAM
edits continue. A cleanup pass has a fixed cutoff; newer checkpoints queue the
next cutoff instead of restarting it.

**Example:** while an index on saved `orders(customer_id)` waits for storage, a
cached order lookup and another connection's commit can finish. The index
becomes a saved root only after its complete output uploads and the matching
manifest publishes. Replacing its table or changing its inputs abandons the old
job and releases its read request.

`REINDEX` rebuilds from rows, not from the possibly damaged saved index, using
the selected table shape. `REINDEX TABLE CONCURRENTLY orders` follows
PostgreSQL's phases: commit replacement catalog entries, wait for earlier
writers, build, validate against later writes, wait for earlier readers, swap
names and constraint links, then retire and drop the old indexes. A primary-key
constraint keeps its identifier and comment; its backing index gets a new one.
Cancellation can leave a committed invalid replacement or retired index, as in
PostgreSQL. Concurrent rebuilding of an exclusion index is rejected; a table
rebuild warns and skips exclusion indexes and live invalid indexes. Schema and
database rebuilds finish one table at a time, and their end hooks report each
index's current name. Keys keep the name chosen at creation: renaming `orders`
to `archived_orders` preserves `orders_pkey`. Changing a table's owner changes
its live indexes' owners in the same transaction. See the
[compatibility guide](postgresql-18-compatibility.md) for supported boundaries.

Application jobs, migration scheduling and backup schedules are operator work.
There is no built-in SQL job scheduler or PostgreSQL background-worker ABI.
See [operations](operations.md#scheduled-work-and-schema-migrations) for examples
and [backup rules](backup-restore.md) before scheduling an offline backup command.

## Memory and physical size

Every pool below is reserved at startup and never grows. Exhaustion returns
`54000` naming the pool or its settings, with requested bytes, free bytes, the
largest free extent and remaining lease descriptors where applicable. Byte
pools must be positive multiples of 64; each range rounds up to 64 bytes, and
fragmentation or old/new overlap during a replacement can exhaust bytes or
descriptors independently. File configurations must name each byte/lease pair
explicitly.

### Shared memory pools

| Setting | What it holds | Notes |
|---|---|---|
| `row_overlay_slots` | Resident row metadata for ordinary tables in all databases | Not multiplied by `max_tables`. Saved rows stream from immutable generations; only resident changes, uncheckpointed journal replay and needed history use slots |
| `large_object_pages` | The internal large-object table | Separate from `row_overlay_slots` |
| `row_history_slots` | MVCC row history across all tables and large objects | `max_row_versions_per_row` is a separate per-row bound. An update owns its history slot before durability; on pressure, admission examines at most 64 row metadata positions, then returns the named error. Locking mode reserves none |
| `row_work_bytes` / `row_work_leases` | Retained row-ID lists, DML buffers and saved procedural row state | Read captures reserve their known row count; write buffers grow geometrically within the pool, and a moving buffer keeps both ranges charged until copied. An update and a long report compete for the same budget |
| `execution_control_bytes` / `execution_control_leases` | Procedural positions, frames, source images, local declarations and value slots, named-output bindings, saved outputs, caught error directories and text | Ranges are sized to actual source text and maximum lexical, loop and handler nesting, and admitted before local initializers. Startup accounting includes lease and free-range records |
| `table_metadata_bytes` / `table_metadata_leases` | Actual-length columns, physical column numbers, constraints, pending edit mappings, index bindings, saved checked SQL inputs, TRUNCATE identities, prepared value-index keys | Starter: 64 MiB and 8,192 leases. Keys use one range per distinct row, so 8,192 leases is not an 8,192-row transaction limit. Exhaustion fails before the journal becomes durable |
| `statistics_bytes` / `statistics_leases` | Analyzed-column statistics images | Starter: 1 MiB and 1,024 ranges. `ANALYZE orders(total)` merges that column with existing records and reserves the replacement before publishing |
| `index_statistics_slots` | Index activity counters | Starter: 4,096. Reserved at index creation; a table without indexes owns none |
| `definition_image_slots` | Retained execution-schema identities | A paused trigger pins its completion's existing image rather than reserving another |
| `result_spool_bytes` / `result_spool_leases` | Result rows queued behind clients that read slower than statements produce them | A connection holds at most one lease, growing geometrically and returned once its queue drains. Exhaustion fails only the lagging statement with `54000` after the rows already queued |
| `plan_build_bytes` / `plan_build_leases` | Build sides of retained plan nodes: a retained hash join's buckets, entries and encoded build rows | Starter: 16 MiB and 64 leases; a hash join holds three leases and a sort two, from opening (a cursor's from `DECLARE`) until the stream or cursor ends. A build that does not fit runs its statement as it ran without streaming: the result spools, a cursor saves its rows |
| `memtable_bytes` | Resident row heap | At most 4,294,967,295 bytes per buffer, two buffers reserved; a cache limit, not total table size |

**Example:** a function with 128 declared locals and one named input stages
that one parameter, then releases the staging range. It does not reserve a
second 129-value array; larger text or record payloads live in statement or
row-work memory.

### Per-connection and per-transaction reservations

| Setting | Reservation |
|---|---|
| `prepared_bytes`, `max_prepared` | One `prepared_bytes` buffer per prepared-statement slot plus one for active input preparation: `max_prepared=8` reserves `17 * prepared_bytes`. Text, parameter declarations and checked literals share each buffer |
| `routine_input_slots` | Plan-control records per backend (starter: 128), also bounding watched plans. Values lease actual bytes from `table_metadata_bytes` |
| `max_ddl_per_transaction` | DDL event controls per transaction; actual-length records use `table_metadata_bytes` |
| `cursor_query_bytes` | Per-cursor query state, including the declared type IDs a `WITH HOLD` cursor keeps after commit |
| `cursor_bytes` | Per-cursor typed output rows, sharing their bytes and `floor(cursor_bytes / 6)` row positions with the saved input rows a scrollable cursor evaluates again or an ordered row-locking cursor sorts |
| `conn_send_buffer_bytes` | Reserved twice per connection: the send buffer and the private output reserve that holds a result's closing messages. The largest single backend message must fit it |
| `result_stream_bytes`, `result_stream_slots` | `result_stream_slots` result streams per connection of `result_stream_bytes` each; both required (starter: 256 KiB, minimum 64 KiB, and 2 slots, at least 1); zero bytes disables streams. The estimate counts every resident row of the table, so a table with many uncheckpointed rows spools instead |
| Held notifications | 64 KiB per connection for notifications that arrive while a result is in progress; a listener further behind is closed rather than sent a truncated stream |

Tables store their actual columns, not a maximum-width array; dropped columns
keep their PostgreSQL column numbers. Committed schemas, pending edits and
retained readers share immutable images, and old and new metadata stay charged
until their last owner releases them. Catalog capacities are independent:
changing `max_tables` does not resize indexes, routines, views or extended
statistics. SERIAL and identity columns use the shared sequence catalog. No
controller resizes startup backing according to load; the
[adaptive resource design](adaptive-resources.md) describes future policies.

### Physical bounds

Capacity is the smallest applicable bound, not the size of the bucket:

| Boundary | Current bound and practical meaning |
|---|---|
| Encoded immutable block | At most 262,144 bytes including its 52-byte header; payload at most 262,092 bytes |
| Saved row overflow | An inline head plus at most 16 overflow blocks; assembled encoded row at most 4,455,030 bytes. Input, statement and result buffers can impose smaller bounds |
| External execution-run row | At most 262,072 encoded bytes without overflow chaining, before other statement-memory limits |
| One row-generation roster | At most 8,190 referenced physical blocks, excluding the roster itself; indexes, filters, descriptors and overflow consume entries too |
| Index navigation within a generation | At most 4,096 index leaves; leaf and root addressability do not override the smaller physical-block roster |
| Resident row heap | `memtable_bytes`, above |
| Table catalogs | `max_tables` at most 10,000 user table slots across the instance, plus the internal large-object relation; materialized-view backing tables consume slots |
| Global saved-block inventory | `checkpoint_live_blocks` must cover the distinct reachable block identities, including retained readers and backup data |
| Additional startup bounds | Manifest bytes, replayed commit batches, generations per table, merge/write entries, locks, transaction changes, connections and statement memory can fill first |

The 8,190-block roster gives a gross upper bound of about 2 GiB of framed
referenced block bytes for one generation if all blocks are full. It does not
promise a 2 GiB user table: metadata consumes entries, blocks can be short,
compression changes logical size, and a table can use several generations.

**Sizing example:** `checkpoint_live_blocks = 65536`, as in the development
configuration, corresponds to at most 16 GiB of full 256 KiB referenced block
bytes in that inventory. Small packed objects, metadata, indexes, historical
versions and backups reduce the useful row-data share. It is not a demonstrated
16 GiB database capacity or a provider limit, and a larger bucket alone does
not fix an exhausted inventory.

A single large transaction, or a large uncheckpointed journal tail replayed at
startup, can exhaust resident memory while the durable table is small.
PostgreSQL-specific width errors keep their own codes; the
[capacity inventory](postgresql-18-compatibility.md#capacity-boundaries) owns
exact SQL widths and per-workspace reservations. There is no qualified maximum
database size, bandwidth or transaction rate; the
[performance guide](performance.md#estimating-bandwidth-and-throughput) gives
explicit calculations and dated measurements.

### Measuring startup memory

The [embedded memory helper](https://github.com/e6qu/pos3ql/blob/main/examples/embedded-memory.rs)
and [runner script](https://github.com/e6qu/pos3ql/blob/main/tools/measure-embedded-capacity.py)
measure 1, 8, 32 and 128 configured tables with the same explicit shared
capacities in each transaction mode, across 64 samples (empty, hot, even,
moving-demand, 64-column, schema/index replacement and statistics workloads).
Each sample records named startup reservations, configured reservation, peak
resident memory, the 128 MiB virtual stack used by the server and native tests,
and startup time. Results count only when collected on CI; the single session
does not measure pinned-reader pressure. Historical debug-build baselines, which
do not measure the current implementation or a production minimum:
[empty engine](https://github.com/e6qu/pos3ql/blob/6ff1ac865a86a5318cb4934fbf01be0926667811/benchmarks/baselines/2026-10-30-embedded-startup/README.md),
[component breakdown](https://github.com/e6qu/pos3ql/blob/6ff1ac865a86a5318cb4934fbf01be0926667811/benchmarks/baselines/2026-10-30-embedded-components/README.md)
and [lazy definitions](https://github.com/e6qu/pos3ql/blob/4054a6d3c79e916728f8e84d84139eae6d86b501/benchmarks/baselines/2026-10-30-embedded-lazy-definitions/README.md)
(empty-engine peak fell from about 735 MiB to 431 MiB with 32 table slots).

## Schema migrations and binary changes

Application schema migrations use supported SQL through a normal client.
Changes such as ADD COLUMN, CREATE INDEX and type rewrites have their ordinary
locks, memory requirements and validation costs. Transactional DDL and savepoints
do not make a migration nonblocking or make an arbitrarily large rewrite fit.
A migration tool must coordinate which client applies each version; pos3ql does
not supply a migration runner or migration-history table.

Changing the pos3ql binary is separate. Earlier pos3ql format compatibility is
not required and there is no automatic format migration. Read the
[format matrix](durable-format.md), retain a recoverable source and verify a
logical export/restore when formats differ. A same-prefix backup does not
convert its bytes to a new format. PostgreSQL physical backup/WAL tools cannot
read pos3ql objects.

## Source map

| Source | Responsibility |
|---|---|
| `src/server.rs`, `src/pg/` | Connections, wire protocol, readiness events, retained waits and response order |
| `src/sql/` | Parsing, type checks, execution, transaction coordination and PostgreSQL catalogs |
| `src/storage/` | Resident versions, locks, retained definitions, object lifetimes and fixed metadata pools |
| `src/wal/` | Journal record framing, checksums, staging and recovery codec |
| `src/checkpoint.rs` | Saved generations, merges, manifest publication, retention and cold recovery |
| `src/store/` | Block formats, compression, immutable indexes, ranged access and cache tiers |
| `src/object_store.rs`, `src/object_store/` | Provider-neutral outcomes, S3 HTTP/TLS, signing and conditional operations |
| `src/mem/`, `src/config.rs` | Startup sizing, fixed buffers/pools and allocation enforcement |

Unfinished work is tracked in [PLAN.md](../PLAN.md#milestones).
