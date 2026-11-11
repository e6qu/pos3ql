# What works today

This guide describes application-visible behavior you can rely on now. The
[architecture](architecture.md) explains how the server and object storage
work; [compatibility](postgresql-18-compatibility.md) owns the exact SQL surface
and [capacity limits](postgresql-18-compatibility.md#capacity-boundaries).
Unfinished work lives only in [PLAN.md](../PLAN.md).

## What an application can do

| Task | Current behavior | Detailed guide |
|---|---|---|
| Connect with PostgreSQL clients | SQL, transactions, catalogs, drivers and dump/restore are compared with PostgreSQL 18 | [Compatibility](postgresql-18-compatibility.md) |
| Save an order outside the server | In object-store mode, success waits for durable log publication | [Object storage](object-storage.md) |
| Read selected rows and columns | Immutable indexes and selected-column reads can reduce storage traffic | [Architecture](architecture.md#reads-and-storage-layout), [indexes](index-navigation.md) |
| Recover without local caches | Checkpoint objects plus later committed journal batches rebuild the database | [Recovery](backup-restore.md) |
| Copy selected table changes | Logical subscribers keep separate, asynchronously updated databases | [Replication](logical-replication.md) |
| Operate and replace a server | Readiness, metrics, credential reload, packages and passive-candidate promotion | [Operations](operations.md) |
| Query documents | JSON, JSONB, JSON paths and SQL/XML in their stated forms | [JSON](sql-json.md), [XML](sql-xml.md) |

SQL runs on one CPU worker. Several storage requests can be in flight, and
cached work can run while an eligible query waits for storage. Some paths still
block that worker; see [Pause and resume every query path](../PLAN.md#pause-and-resume-every-query-path)
and [Run SQL on several CPU workers](../PLAN.md#run-sql-on-several-cpu-workers).

## Transactions and durable success

Every configuration sets `transaction_mode`, `catalog_history_bytes` and
`row_history_slots` explicitly:

| Mode | Reader and writer behavior | Isolation | History settings |
|---|---|---|---|
| `mvcc` | Readers keep seeing earlier values while they need them | PostgreSQL snapshot and isolation behavior | Positive |
| `locking` | Readers see current committed values; conflicting relation operations wait | Read committed (and read uncommitted, which PostgreSQL treats the same); repeatable read, serializable and exported snapshots return `0A000` | Zero |

Rollback, savepoints and private uncommitted changes work in both modes.
Row-history slots are shared by all tables and large objects; an update
reserves its slot before commit and rollback releases it. A long report can
hold history until the fixed pools fill, at which point the server returns a
named capacity error instead of growing memory.

**Example:** in MVCC mode, a repeatable-read report keeps seeing an order's
earlier status after another transaction changes it. In locking mode, a
conflicting writer waits for the reader's relation lock, while work on
unrelated tables continues.

A commit becomes visible all at once, and only after it is durable: new
statements never see half of an order-and-invoice update. If commit waits for
storage, completed work is not repeated; a default that issued an order number
and a trigger that wrote an audit row do not run again. Commit can be cancelled
until it is published.

Prepared transactions keep their locks and private catalog changes across a
restart. For example, a domain created before `PREPARE TRANSACTION 'import'`
stays hidden from other connections until `COMMIT PREPARED 'import'`; rolling
it back removes the domain.

A lost final reply leaves the client unsure whether the commit happened. Check
application state before retrying an operation that could charge twice. If the
server cannot install a commit after it became durable, it rejects further work
with `08007` and requires restart and recovery; it never undoes durable changes.

## Temporary objects and connection cleanup

Temporary tables, views and sequences belong to the connection that created
them and are never written to object storage. Named types and routines require
ordinary schemas. `DISCARD TEMP` drops temporary objects with the same
dependency checks as `DROP`, and rollback restores them:

```sql
CREATE TEMP TABLE import_rows (id integer);
INSERT INTO import_rows VALUES (7);
BEGIN;
DISCARD TEMP;
ROLLBACK;
SELECT id FROM import_rows; -- returns 7
```

`pg_temp` qualifies table names; a `regnamespace` cast needs the actual schema
name from `pg_namespace`. Outside a transaction, `DISCARD ALL` also clears
prepared statements, cursors, session settings, notifications, sequence session
state and session advisory locks. `DISCARD SEQUENCES` forgets this connection's
remembered sequence values without rewinding the sequence. `DISCARD PLANS`
affects future preparation; open portals and cursors keep their checked values.

## Account identity and permissions

A connection follows the account that logged in, including through renames:

```sql
ALTER ROLE orders_app RENAME TO orders_writer;
CREATE ROLE orders_app LOGIN;
```

The existing connection becomes `orders_writer`. The new `orders_app` gets none
of its permissions or cancellation rights. If the original account is dropped,
simple constant queries still work, but `current_user` and `session_user`
return `42704` and object creation returns `42501`. Recreating a dropped owner's
name does not grant the right to resolve its prepared transactions; superusers
still can.

Memberships and grants follow PostgreSQL:

- Grants by different accounts are independent; revoking one keeps the others.
  Repeating a grant emits a NOTICE; revoking a missing one emits a WARNING.
- Changing ADMIN, INHERIT or SET keeps the membership identifier; revoking and
  regranting creates a new one.
- Owner privileges follow inherited membership, separately from `SET ROLE`.
  With INHERIT FALSE and SET TRUE for `owners`, an `app` account must run
  `SET ROLE owners` before altering their table. With INHERIT TRUE and SET
  FALSE, it can alter the table directly, but `SET ROLE owners` returns
  `42501`. The same applies to default privileges and `DROP`/`REASSIGN OWNED`.
- Revoking an owner's own SELECT does not remove its right to grant SELECT.
- Default privileges for future objects keep their identifier while any grant
  remains; resetting to the ordinary defaults removes the row. Dropping a
  schema removes its default privileges.

Permission and membership changes on one object or role wait for that object's
open transaction; other objects proceed. Grant order and explicitly empty
permission lists survive restart.

Checking privileges on a missing object by OID follows PostgreSQL: a missing
table, sequence or large object returns NULL; a missing type, schema, function,
language, database, tablespace, foreign-data wrapper or foreign server returns
true for a superuser and NULL otherwise. Missing names and invalid privilege
names raise errors.

## Reads keep their selected data

A query that pauses for storage keeps the rows, definitions and saved files it
selected. Concurrent updates, table drops and background maintenance cannot
redirect it to replacement data; a new query sees the replacement.

**Example:** a report pauses after reading one order. Another connection updates
an unread order and maintenance replaces saved files. The resumed report still
returns its original values.

Large results send one header and one completion, over the simple protocol
and an unpaged extended `Execute` alike. A client that reads slowly never
stalls the server: rows that do not fit the send buffer queue in the shared
result spool, other sessions keep running, and the slow connection continues
with its next message once its client has read the result.

**Example:** a reporting tool runs `SELECT * FROM orders` over a slow link and
reads it gradually. Meanwhile other applications keep inserting orders. If the
tool falls behind by more than `result_spool_bytes`, it receives the rows
already queued followed by `54000` naming that setting, and its session stays
usable. A large `SELECT` from one ordinary table or one builtin series such
as `generate_series`, without sorting, grouping or joins, resumes row by row
instead: it waits for the client and for cold storage without holding the
worker, and rows already delivered are never repeated. A paged portal (an
extended `Execute` with a row limit) over such a query evaluates rows only as
each `Execute` asks for them, as PostgreSQL does, so
`CASE WHEN id > 1 THEN nextval('s') END` takes nine numbers for a first page
of ten rows.
Other large results, once their first byte reaches the client, read cold
storage without yielding; extending resumable output to them is tracked in
[the plan](../PLAN.md#pause-and-resume-every-query-path).

A physical table rewrite follows PostgreSQL's
[MVCC caveat](https://www.postgresql.org/docs/18/mvcc-caveats.html): an older
repeatable-read transaction that first opens the table after the rewrite can
see it as empty.

### When volatile expressions run

Sorting postpones output expressions until the result order is known:

| Query | `nextval` calls |
|---|---|
| `SELECT id, nextval('order_numbers') FROM orders ORDER BY id LIMIT 1` | 1 |
| Same with `OFFSET 1` | 2 (PostgreSQL also evaluates the skipped row) |
| Same with `FETCH FIRST 1 ROW WITH TIES` | 2 (the next row is checked for a tie) |
| `ORDER BY nextval(...)` | one per candidate row, reused for output |
| Sorted subquery with `LIMIT 0` | 0 |

`LIMIT` bounds returned rows, but expressions evaluated before sorting can still
fail on later input rows. Spilling a sort to temporary disk does not change
these effects. Sorting also keeps whether a joined row existed:
`row_to_json(profile)` distinguishes a missing profile from one whose fields are
all null.

`GREATEST`, `COALESCE`, `BETWEEN` and JSON and XML constructors evaluate their
arguments as PostgreSQL does. `1 BETWEEN 2 AND NULL` is false, and a sequence
call inside `BETWEEN` can issue more than one value.

### Comparisons and conversions

- `NULLIF` uses the selected equality operator: `pg_typeof(NULLIF(1, 2.2))` is
  `numeric`.
- `real` and `double precision` convert to `numeric` with six and fifteen
  significant digits: `1.2345678::real::numeric` is `1.23457`.
- Geometric casts follow PostgreSQL: assigning a polygon to a path column is
  allowed; a box to a point column needs `::point`. Converting an open path to a
  polygon fails with `22023`.
- Registered cast functions receive the type modifier and explicit flag:
  `SELECT 7::varchar(5)` passes `9` (length plus four) and true; inserting `7`
  into a `varchar(5)` column passes `9` and false. Array and domain element
  conversions keep the modifier.
- Array casts prefer a registered whole-array cast, then convert elements once
  each in order, keeping dimensions and lower bounds.
- `array_eq`, `array_lt` and the other array comparison functions, including
  `btarraycmp`, match the SQL operators and accept domains over arrays. Null
  elements compare equal and sort after non-null elements:
  `ARRAY[NULL::integer] > ARRAY[1]` is true.
- SQL JSON constructors use registered enum-to-JSON casts, including inside
  arrays and records.

## Cursors

SQL cursors save typed values and format each fetched row with the session's
current settings. A saved `DATE '2020-01-02'` displays as `02/01/2020` after
`SET DateStyle = 'SQL, DMY'`, even in a cursor held across commit. Procedural
`FETCH INTO` assigns the typed value directly. Opening a procedural cursor
captures its inputs: changing `label` after `OPEN c FOR SELECT label` does not
change the cursor. Dynamic SQL sees only the explicit `USING` arguments.

### Cursors that read as you fetch

A cursor over one table, over constant `VALUES` rows, or over a builtin
set-returning function with constant arguments (literals, parameters and
casts of them) reads input only as rows are fetched when
its query uses only columns, constants, parameters,
built-in operators and casts, boolean logic, `CASE`, `BETWEEN`, null tests,
array construction, subscripts, slices, scalar function calls, `LIMIT` and
`OFFSET`. A cursor over one table, including a row-locking cursor (`FOR
UPDATE`, `FOR SHARE` and their variants), may also order by its columns. Other
cursor queries, including orderings by other expressions, user-defined
operators or row-level security, run when declared; see
[the plan](../PLAN.md#pause-and-resume-every-query-path). The functions are
`generate_series` (integer, bigint, numeric or timestamp),
`generate_subscripts`, a one-array `unnest`, `regexp_matches`,
`regexp_split_to_table`, and the `json`/`jsonb` `_each`, `_each_text`,
`_array_elements`, `_array_elements_text` and `_object_keys` functions, alone
or combined in `ROWS FROM (...)` (which `FROM unnest(a, b)` expands to), with
or without `WITH ORDINALITY`. Such a cursor keeps only the arguments and the
next position; argument errors, such as `jsonb_each('[1]')`, are raised by the
first `FETCH`. Other function sources, including column definition lists such
as `jsonb_to_recordset(...) AS t(a int)`, run when declared.

```sql
DECLARE c NO SCROLL CURSOR FOR SELECT id, label FROM orders;
FETCH 2 FROM c;  -- reads two rows
DECLARE parts NO SCROLL CURSOR FOR
  SELECT key, value FROM jsonb_each('{"a": 1, "b": 2}');
FETCH 1 FROM parts;  -- produces only the "a" row
```

- `FETCH 0` before the first row reads nothing; `MOVE` skips without saving
  output. Each batch reuses the result buffer, so many small fetches do not
  accumulate memory. A forward `FETCH n` or `FETCH ALL` sent by the client
  from a `NO SCROLL` cursor saves no rows at all: each row is evaluated as the
  client reads it, like a large `SELECT`, so `FETCH ALL` returns any number
  of rows and other sessions keep running while the client reads slowly.
- `SELECT 100/id FROM orders WHERE id<>0` divides only fetched rows.
  `CASE WHEN id<>0 THEN 100/id END` evaluates only the chosen branch.
- `DECLARE c NO SCROLL CURSOR FOR SELECT nextval('order_numbers')` issues no
  number until the first nonzero fetch; the same holds for
  `SELECT nextval('order_numbers') FROM generate_series(1, 3)`. A series keeps
  only its next value, so `generate_series(1, 70000)` costs no row memory
  until rows are fetched, and its arguments (a zero step, for example) are
  checked by the first fetch, as in PostgreSQL. Other functions in `FROM`
  still run when the cursor is declared.
- Type errors and constant errors such as `1/0` are reported at `DECLARE`, even
  for empty input. Operator choices are fixed then; later `search_path` changes
  do not affect the open cursor.
- `LIMIT 2 OFFSET 3` skips three matching rows and returns at most two.
  Negative bounds fail when fetching starts. Skipped rows still evaluate the
  output list, so a division by zero in a skipped row fails.
- Constant false or null filters, including `'f'` and captured parameters, read
  nothing.
- Under serializable isolation, a table counts as read only once fetching reads
  it.
- Selecting `id` from a row with a 128 KiB text column does not copy that text
  into cursor memory.

`SCROLL` cursors save visited rows so `FETCH PRIOR` and jumps backward need no
new reads; `FETCH LAST` reads the rest. When the output list calls a volatile
function such as `nextval`, a row reached again by `FETCH PRIOR`, `ABSOLUTE`,
`RELATIVE` or a later forward fetch is evaluated again, as PostgreSQL scans
backward: with `SELECT id, nextval('s') FROM t`, `FETCH NEXT` twice then
`FETCH PRIOR` returns row 1 with the third number. The cursor then saves the
input columns of each row instead of its output, and a volatile filter also
saves the rows it rejected, because the filter runs again too. `LIMIT` returns
the row at the edge of its window again without evaluating it, and `MOVE` and
`FETCH ABSOLUTE` take PostgreSQL's steps (an absolute target less than halfway
back rewinds and reads forward). History must fit `cursor_bytes`; exhaustion
returns `54000` and leaves the cursor failed until closed. Without a scroll
option, a table scan, a retained set-returning function and a list of two or
more `VALUES` rows allow scrolling; one row, `UNION ALL` and a provably empty
result are forward-only and return saved rows. `pg_cursors.is_scrollable`
reports the choice made at declaration.

A row-locking cursor locks each row when a `FETCH` or `MOVE` reaches it, not
at `DECLARE`, after evaluating its output list and before `OFFSET` counts it:

```sql
DECLARE c CURSOR FOR SELECT id, v FROM orders ORDER BY id FOR UPDATE;
FETCH 1 FROM c;  -- locks only the first row
```

An ordered cursor sorts at its first `FETCH`, as PostgreSQL's sort reads its
whole input then: it saves the columns it reads of every row its filter
passes in `cursor_bytes` (a full buffer returns `54000` before any row is
returned or locked) and sorts them, keeping scan order for equal keys. Each
row's output list is then evaluated when a fetch reaches it, so
`SELECT v, nextval('s') FROM t ORDER BY id` takes numbers in sorted order, and
a scrollable ordered cursor evaluates a revisited row's volatile output again
without running its filter again. A row-locking ordered cursor saves whole
rows. A `FETCH` that
meets a row locked by another transaction waits like `SELECT ... FOR UPDATE`;
`lock_timeout`, `statement_timeout` and cancel end the wait. A row changed by a
transaction that committed after `DECLARE` is checked again: under `READ
COMMITTED` its newest version is filtered and projected again, or skipped if it
was deleted or no longer matches; under `REPEATABLE READ` and `SERIALIZABLE`
the fetch fails with `40001`. Rows the cursor's own transaction changed after
`DECLARE` are skipped. Such cursors are never scrollable by default.

`WITH HOLD` saves the unread rows at commit, including when declared outside
`BEGIN`. A scrollable cursor that evaluates revisited rows again is rewound and
saved whole, evaluating every row again; it keeps its position. Rolling back a later transaction that fetched from it does not rewind
or close it. If saving fails — for example, an unread row with `id=0` makes
`SELECT 100/id` fail at `COMMIT` — the commit rolls back and closes new
cursors; cursors held earlier survive. Unread rows must fit `cursor_bytes`.

While such a cursor is open, the same session cannot `DROP`, `TRUNCATE` or
structurally `ALTER` its table; renaming the table is allowed.

## Writes

UPDATE, DELETE, MERGE, `ON CONFLICT` and validation keep the selected row
values. If the table or its definition changed before the write is installed,
the statement returns `40001`. A full rollback log rejects only the change it
could not record. ALTER that leaves the row representation unchanged does not
rewrite rows.

## COPY

`COPY TO` in table form reads only that table; use query form to include
partitions, views or an explicit order:

```sql
COPY (SELECT id, status FROM orders ORDER BY id) TO STDOUT (FORMAT csv);
```

Table-form `COPY TO` returns `42809` for a partitioned parent without an
applicable row-security rewrite and `0A000` for an unpopulated materialized
view. Views, foreign tables and catalog tables require query form. Otherwise
it sends the rows of `SELECT columns FROM ONLY table`, in the same order a
`SELECT` without `ORDER BY` returns them. A large `COPY ... TO STDOUT` from
one table or a streamable query resumes row by row like a large `SELECT`:
it waits for a slow client and for cold storage without holding the worker.

`COPY FROM` statement triggers declared with
`REFERENCING NEW TABLE AS inserted_rows` see the values saved after BEFORE
triggers and generated columns: `INSERT INTO audit SELECT id FROM inserted_rows`
reads that COPY's rows. AFTER row triggers cannot change them. These rows use
`copy_transition_bytes` plus `txn_rows` records reserved per connection and per
subscription worker; exhaustion returns `54000` and aborts the COPY.

Deferred constraints run before every commit, including after COPY and at an
extended-protocol Sync. A failed check rolls back the command instead of
confirming a partial import. COPY keeps its original user, `search_path`,
formatting settings, timestamps and statement deadline across data messages.

## Functions, sequences and retries

Ordinary `nextval` is not rolled back; a transactional restart is:

```sql
CREATE SEQUENCE order_numbers START 10;
SELECT nextval('order_numbers'); -- 10
BEGIN;
ALTER SEQUENCE order_numbers RESTART WITH 50;
SELECT nextval('order_numbers'); -- 50
ROLLBACK;
SELECT nextval('order_numbers'); -- 11
```

When a command waits and resumes, completed effects are not repeated. A
PL/pgSQL or SQL-language function whose audit INSERT completed runs it once,
even if its next SELECT waits repeatedly for a lock. Sequence values issued
before a wait are reused; if recording them exceeds statement memory, the
command fails with `54000`, `volatile sequence replay exceeds statement memory`,
before an unrecorded sequence change. Functions that change state can still
block the SQL worker on an uncached read; see
[the plan](../PLAN.md#pause-and-resume-every-query-path).

Evaluation order follows PostgreSQL:

- Skipped `CASE` and procedural branches do not run, but ordinary SQL can still
  prepare some uncorrelated subqueries in a skipped branch.
- Immutable constant expressions can fail during planning even when a false
  filter removes every row, following PostgreSQL's
  [volatility rules](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/include/catalog/pg_proc.dat).
  Volatile calls, such as `brin_summarize_new_values('orders_brin')`, run only
  when reached.
- A `STRICT` scalar function with a constant null argument is skipped without
  evaluating its other inputs, including in inserted defaults and an explicitly
  null variadic array. A null element inside a variadic array does not skip it.
  A strict function in `FROM` evaluates its inputs first.
- Given `import_batch(bigint, bigint)`,
  `CALL import_batch(nextval('order_numbers'), 'bad')` fails before issuing a
  number, while `CALL import_batch(nextval('order_numbers'), 1/0)` issues one.
- Set-returning functions evaluate their inputs once.
- Constant catalog casts validate without input rows: selecting
  `'missing_table'::regclass` from an empty table raises `42P01`.

## Procedural code

- A trigger can end in a nested `RETURN` or `RAISE`. Reaching the end without
  returning fails with `2F005` and rolls the command back.
- Procedural expressions can run subqueries over current variables and trigger
  rows, such as `IF EXISTS (SELECT 1 FROM orders WHERE id=NEW.id)`. A scalar
  subquery with no rows is NULL; more than one row fails with `21000`.
- A numeric `FOR` loop declares its own integer variable, leaving an outer
  `value text` unchanged. Query `FOR` loops and `FOREACH` assign the declared
  target, which keeps the last value. An empty query loop assigns a null row;
  an empty array loop leaves the target unchanged.
  [PostgreSQL reference results](https://github.com/e6qu/pos3ql/blob/bcbaba7ba47b735c123198a03d14e80935340a3e/tests/external/reference/2026-11-03-input-plans/README.md)
  show these cases.
- `EXCEPTION WHEN OTHERS` does not catch cancellation or assertion failures;
  name `query_canceled` or `assert_failure` explicitly.
- A caught block is a subtransaction. Its row changes, schema edits, settings
  and queued deferred triggers are undone; local variables and issued sequence
  values are kept. After a failed `ALTER TABLE orders ADD COLUMN note text`, the
  handler sees the original table. Cursors opened inside the failed block close;
  older cursors keep their position. See the
  [PostgreSQL reference results](https://github.com/e6qu/pos3ql/tree/854feb372391d1c87f3d5b261e85a78430d96449/tests/external/reference/2026-11-05-procedural-subtransactions).
- DDL succeeds only after its end-of-command event triggers. A failing hook for
  `DO 'BEGIN CREATE TABLE orders(id integer); END'` leaves no `orders` table and
  sends no `DO` success tag. Static DDL in routines uses the same path as
  dynamic `EXECUTE`.

### Statement deadlines

A statement keeps one deadline through waits and nested SQL. Extended-protocol
Parse, Bind, Describe and Execute share the timer until Execute or Sync
finishes, so pausing between Bind and Execute does not buy more time. An
expired timer is reported on the next message, including Flush, Close or Sync.
Procedural code checks the deadline at every block and instruction, including
empty loops. During COPY FROM, a row arriving after the deadline fails with
`57014`. The
[PostgreSQL probe](https://github.com/e6qu/pos3ql/tree/02d030e49e5a483d567ffb205dd01c4b9faad857/tests/external/reference/2026-11-05-procedural-deadlines)
records these boundaries.

## Schema changes and object identity

Renames keep an object's identifier (OID); drop and recreate always assign a
new one. This covers relations, types, enum members, indexes, constraints,
statistics, extensions, foreign-data objects, publications, subscriptions,
collations, conversions, text-search objects, casts, operators and their
classes and families, access methods, event triggers, memberships, default
privileges, triggers, rules, routines and row-security policies.

```sql
CREATE TABLE orders(id integer);
CREATE TABLE saved_order_identity AS SELECT 'orders'::regclass::oid AS identity;
DROP TABLE orders;
CREATE TABLE orders(id integer);
SELECT has_table_privilege(identity, 'SELECT') IS NULL FROM saved_order_identity;
-- true: the old identifier does not grant access to the replacement
```

Capture an object's OID before dropping it if you want to check that its
catalog rows are gone.

- A schema change on one table waits for that table's locks; unrelated tables
  stay available.
- Dropped columns keep their numbers; new columns append. The 1,600-column limit
  counts dropped columns.
- Removing a column or rule does not renumber the others.
  `ADD CONSTRAINT ... USING INDEX` keeps the index's identifier. NOT NULL
  constraint names survive table and column renames.
- `UNIQUE NULLS NOT DISTINCT` treats two keys with NULLs as duplicates;
  `NULLS DISTINCT` allows them.
- Copies made from a database template keep the template's local identifiers
  but edit independently: renaming an operator class in `reporting` does not
  rename it in `template1`.
- Inherited triggers keep their identifier when disabled or renamed; detaching
  and reattaching a partition creates a new one. Each child keeps its own
  enabled mode, so `ALTER TABLE ONLY orders DISABLE TRIGGER audit_order` leaves
  children's copies enabled. A parent's statement trigger fires once and is
  not inherited.
- Operator classes and families can name built-in and user operators by exact
  argument types. `pg_catalog.<` picks the built-in operator; plain `<` follows
  `search_path`.

Bound references keep their original target:

- A paused comparison keeps its collation even if that collation is dropped:
  with a copy of `pg_unicode_fast`, `upper('straße')` is still `STRASSE`.
- Renaming enum label `pending` to `waiting` changes its display without
  rewriting rows. A new `pending` label cannot capture old constants.
- Moving a table to another schema keeps bound references. String SQL bodies
  and text-based lookups keep their text and can fail after a rename.
- `pg_type.typdefault` can keep the original text for a domain;
  `pg_get_expr(typdefaultbin, 0)` shows the current expression.

A table or view reserves its row-type name; a named type cannot reuse it.
`search_path` skips schemas without USAGE. Numeric sequence identifiers skip
name lookup but still require sequence permissions.

Validation, including unique index builds, checks all committed rows and the
caller's own changes, so it cannot miss a duplicate committed after an older
report started. `NOT VALID` skips existing rows where supported. Validating a
domain used inside stored arrays or composites returns `0A000`. PostgreSQL's
[table validation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/tablecmds.c),
[domain validation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/typecmds.c)
and [index build](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/heap/heapam_handler.c)
are the reference.

## Views, saved results and rules

`CREATE OR REPLACE VIEW` keeps the view's identifier, row type, `_RETURN` rule,
owner, permissions and comments. Renaming a source column keeps the view's
output names.

A materialized view created `WITH NO DATA` fails ordinary reads with `55000`,
including `LIMIT 0`. REFRESH requires MAINTAIN, runs as the owner and holds an
exclusive lock until the transaction ends. Rollback restores both rows and
populated status.

Rewrite rules take the target table's exclusive lock, so a rule that redirects
an insert conflicts with ordinary reads of that table. `_RETURN` cannot be
renamed. `pg_get_ruledef` supports both formatting modes for unconditional
`DO NOTHING` rules; other rules return their saved SQL, and pretty formatting
of a condition or action returns `0A000`. A missing rule returns NULL.

## Catalog listings and current lookups

Catalog listings follow the query's snapshot: read committed can see newer
rows, repeatable read keeps its earlier listing. Permission checks and role
names use current state, as in PostgreSQL.

**Example:** another connection renames `orders` to `sales`. An older
repeatable-read report still lists `orders`, while `to_regclass('sales')`
resolves the new name. An older `pg_get_viewdef` can show a prior query while
running the view uses the current rule; if the underlying table was dropped,
deparsing can fail with `XX000`. This matches PostgreSQL's
[rule lookup](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/ruleutils.c).

Information-schema views follow the pinned PostgreSQL 18.6
[definitions](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql),
including hiding definitions from non-owners and masking password fields.

- `tableoid` must be named explicitly.
- Index vectors start at subscript zero and keep that bound when cast to
  arrays; casting arrays back returns `42846`.
- `pg_node_tree` values can be copied and deparsed; client text or binary input
  returns `0A000`.
- Some catalog relations resolve by name but return `0A000` when queried.

## Statistics and maintenance

ANALYZE reads committed rows plus the caller's changes. As in PostgreSQL, a
rollback can leave `reltuples` changed while column estimates revert.
`pg_class.relhasindex` is a hint that can stay true after `DROP INDEX`; VACUUM
clears it. `pg_index` lists the actual indexes.

Background maintenance runs between client messages and while idle, and yields
to queries while it waits for storage. It never removes data needed by active
readers, prepared transactions or backups. At 85% heap use, eligible writes can
wait (shown as Extension/CacheCapacity) for maintenance to free space; reads
and commits keep running. Work that cannot fit gets a named capacity error.
See [architecture](architecture.md) for the mechanism and
[operations](operations.md#scheduled-work-and-schema-migrations) for what runs
automatically and what you schedule.

### Numbering changes safely

Journal positions and internal counters never wrap. Running out of journal
positions returns `54000`, `WAL LSN space exhausted`; catalog reservation errors
name the exhausted OID range. Existing objects stay readable. The
[durable format guide](durable-format.md) owns exact encodings and ranges.

## What the tests establish

Allocation guards, fault simulation, recovery after deleting local data,
comparisons with real PostgreSQL and real drivers test these boundaries;
[Contributing](../CONTRIBUTING.md#verification) lists the gates. They do not
establish production performance, complete PostgreSQL compatibility or failover
latency; see the [performance guide](performance.md). Earlier implementation
narratives are in the [history index](history/README.md).
