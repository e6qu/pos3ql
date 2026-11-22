# PostgreSQL 18 compatibility

Use this guide to answer "does feature X work, and what are its limits?" before
assuming that a PostgreSQL application or tool works unchanged. Behavior is
checked against actual PostgreSQL at the SQL, SQLSTATE (the five-character
error code), result, catalog, and wire-protocol boundaries. Anything outside the
tested surface must fail with an error rather than being accepted and ignored.

**User example:** `psql`, a driver, and `pg_restore` work on the tested paths.
A PostgreSQL extension that contains a C library cannot be installed, even
though its `CREATE EXTENSION` command is recognized. The one exception is
`postgres_fdw`, which pos3ql builds in: `CREATE EXTENSION postgres_fdw`
installs it as PostgreSQL does.

The server reports version `18.4 (pos3ql 0.1)`. The vendored PostgreSQL
regression slice is pinned to PostgreSQL 18.6; older test fixtures keep their
exact provenance. [What works today](implemented-baseline.md) summarizes what
applications can rely on;
[transactions, cursors and application behavior](#transactions-cursors-and-application-behavior)
holds the details.

## Supported surface

| Area | Supported |
|---|---|
| Protocol | v3.0/3.2 simple and extended queries, prepared statements/portals, text/binary values, COPY, cancellation, TLS, authentication, notices, notifications, logical replication mode |
| Relational SQL | DDL/DML, MERGE, MVCC transactions, savepoints, two-phase transactions, locks, CTEs, joins, grouping, windows, set operations, views/materialized views, inheritance/partitioning, triggers/rules, cursors, maintenance commands |
| Types | Scalar/array, range/multirange, enum/domain/composite, JSON/JSONB/jsonpath ([SQL/JSON](sql-json.md)), XML ([SQL/XML](sql-xml.md)), temporal, network/geometric, full-text, catalog-reference, ACL, transaction, bit/binary string, UUID |
| Programming | SQL and PL/pgSQL functions, procedures and triggers, `DO` blocks, set-returning/table functions, dynamic SQL, transition/event triggers, privileges, security definer, configuration scopes |
| Catalogs and tools | PostgreSQL-shaped catalogs and monitoring views for tested psql, pg_dump/pg_restore, psycopg2, psycopg 3, SQLAlchemy, Alembic, pgJDBC, Npgsql, node-postgres, and pgx paths |
| Replication | Publication/subscription and pgoutput within the [logical replication boundary](logical-replication.md) |
| Persistence | Object-native commits, checkpoints, recovery, backups, and [versioned durable formats](durable-format.md) |

A recognized command does not imply every grammar production, planner
transformation, or function. `tests/postgresql18_commands.tsv` lists executable
commands and explicit architecture rejections. Differential queries, raw-wire
and driver probes, and the vendored regression schedule cover the rest.
Application fixtures run SQLAlchemy models and Alembic upgrades/downgrades
through psycopg2 and psycopg 3 against both PostgreSQL and pos3ql; they cover
those workflows, not every library feature.

### Behavior details

#### Schemas and catalogs

- User objects cannot be created in `pg_catalog`, `information_schema`, or
  `pg_toast`. `CREATE TABLE information_schema.orders (id integer)` fails with
  `0A000` and is never redirected to `public`. Tables, views, sequences, and
  composite types in `pg_catalog` or `pg_toast` keep PostgreSQL's `42501`;
  missing CREATE permission is also `42501`.
- `information_schema`'s domains (`cardinal_number`, `character_data`,
  `sql_identifier`, `time_stamp`, `yes_or_no`) exist as in PostgreSQL: its view
  columns have those types (`pg_typeof(table_name)` is
  `information_schema.sql_identifier`; the wire type is the base type), casts
  and table columns check them (`(-1)::information_schema.cardinal_number`
  fails with `23514`), and `pg_type`, `pg_constraint` and the domain views list
  them. Arrays of these domains work the same way:
  `'{-1}'::information_schema.cardinal_number[]` fails with `23514`, and an
  element of such an array has the domain's type.
- `pg_type` has PostgreSQL 18.6's 32 columns in its order and lists every
  `pg_catalog` type exactly, pseudo-types (`cstring`, `"any"`) and catalog row
  types included. User-defined types carry the routines and storage traits
  PostgreSQL derives for them (a domain's `domain_recv` and its base type's
  send routine, an array's `array_subscript_handler`). Of `information_schema`'s
  types only its domains and their arrays are listed, not its views' row types.
- `DROP TABLE`, `VIEW`, `MATERIALIZED VIEW`, `FOREIGN TABLE`, `SEQUENCE`,
  `FUNCTION`, `PROCEDURE`, `ROUTINE`, `TYPE`, `DOMAIN`, `SCHEMA`, `OPERATOR`,
  `COLLATION`, `SERVER`, `FOREIGN DATA WRAPPER` and `TEXT SEARCH` objects
  report
  dependent objects as PostgreSQL does, from the rows `pg_depend` lists:
  without `CASCADE`, one `2BP01` error whose `DETAIL` names every dependent
  (`column b of table orders depends on type amount`, `view totals depends on
  column b of table orders`); with it, one `drop cascades to ...` notice for
  the whole statement, in PostgreSQL's order. Indexes, constraints, triggers,
  policies, rules, owned sequences and partitions go silently, and dropping a
  part of another object names its owner (`cannot drop type orders because
  table orders requires it`). A relation of another kind fails with `42809`
  and the hint naming its own `DROP` (`Use DROP VIEW to remove a view.`). The
  walk copies every `pg_depend` row into the statement arena; a catalog whose
  rows do not fit in `sql_arena_bytes` fails the `DROP` with `54000`.
  `pg_depend` has PostgreSQL's rows for partition key columns, PL/pgSQL
  routines' language, index and extended statistics key columns, columns and
  indexes that use a user-defined collation, the columns a rule or trigger
  `WHEN` condition reads through `NEW` and `OLD`, foreign servers, user
  mappings and foreign tables, and the
  parsers, templates and dictionaries text search objects use, so dropping a
  dictionary with `CASCADE` drops the configurations that map it
  (`drop cascades to text search configuration tc`). A partition key column
  or an inherited column cannot be dropped alone (`42P16`).
- `information_schema` views can be queried, but their relation OIDs cannot:
  `'information_schema.columns'::regclass` and `to_regclass(...)` fail with
  `0A000` (`relation OIDs for information_schema views are not supported`).
- Fully qualified catalog columns work, as in
  `SELECT pg_catalog.pg_class.relname FROM pg_catalog.pg_class`.
- `pg_ndistinct`, `pg_dependencies`, and `pg_mcv_list` reject client text and
  binary input with `0A000`, as PostgreSQL does. Copying non-null statistics
  values, including `pg_statistic[]`, into user tables fails with `0A000`.

#### Queries and FROM items

- A `LATERAL` item, or a function's arguments in `FROM`, read only the items
  to its left, never their own or later items' columns: in `SELECT * FROM t,
  LATERAL (SELECT v) s`, `v` is `t.v`. On the right of a `RIGHT` or `FULL`
  join such an item reads none of them; naming one fails with `42P10` and
  PostgreSQL's detail, and a body that names none runs once.
- A comma starts a new join tree: in `FROM x, a JOIN b USING (id)` the
  `USING` and `NATURAL` columns come from `a` and `b` only, and lead their
  tree's columns after `x`'s. `FROM x, a RIGHT JOIN b ON ...` crosses `x`
  with `a RIGHT JOIN b`, and a join condition naming an item before the comma
  fails with PostgreSQL's `42P01` and detail.

#### Data-modifying statements

- A `FROM`, `USING` or `MERGE` source item cannot refer to the statement's
  target, as in PostgreSQL. `UPDATE orders o SET n = 0 FROM generate_series(1,
  o.n) g` and `FROM LATERAL (SELECT o.n)` fail with `42P10`; a plain subquery,
  a join condition or a `MERGE` source fails with `42P01` (`invalid reference
  to FROM-clause entry for table "o"`). An unqualified target column in a plain
  subquery reports `42703` without PostgreSQL's detail line.
- Correlated subqueries in `WHERE` and `WHEN` conditions run for every
  candidate row: `DELETE FROM orders WHERE EXISTS (SELECT 1 FROM refunds r
  WHERE r.order_id = orders.id)`.

#### Identity, inheritance, and partitions

- A `HASH` partition key of an integer, text, `varchar`, `bpchar`, `bool`,
  `"char"`, `oid`, float, `numeric`, date, time, timestamp, `bytea` or `uuid`
  column routes rows with PostgreSQL's seeded hash, and
  `satisfies_hash_partition` answers as PostgreSQL's does
  (`differential_exact/550`); other key types fail with `0A000`.
  `pg_get_partition_constraintdef` and psql's `Partition constraint:` line
  print `generate_partition_qual`'s constraint, and new bounds report
  PostgreSQL's overlap, default-conflict, empty-range and modulus errors
  (`505`).
- A foreign key referencing a partitioned table has one `pg_constraint` clone,
  with its two action triggers, per referenced partition at any depth
  (`pc_a_fkey_1`), kept as partitions are created, attached, detached and
  dropped; at most 256 clones per table (`54000`).
- Outer joins null-extend the rows a partitioned table or inheritance tree
  matches nowhere (`611`); a select list or `UPDATE` over an inheritance tree is
  computed in a `Result` above the `Append`, and `ONLY` writes the parent
  alone (`610`).

- `CREATE TABLE ... (LIKE ... INCLUDING IDENTITY)` creates independently owned
  sequences with the source's settings and starting value; a `serial` default
  copied with `INCLUDING DEFAULTS` or `INCLUDING ALL` shares the original
  sequence. So this copy returns 40 even though the source already issued 40:

  ```sql
  CREATE TABLE source_orders(id bigint GENERATED BY DEFAULT AS IDENTITY (START WITH 40));
  INSERT INTO source_orders DEFAULT VALUES;
  CREATE TABLE copied_orders (LIKE source_orders INCLUDING IDENTITY);
  INSERT INTO copied_orders DEFAULT VALUES RETURNING id; -- 40
  ```

- Ordinary `INHERITS` does not inherit identity generation. Partitions use the
  parent's sequence, but `pg_get_serial_sequence` returns NULL for a child.
  Detaching a partition clears its inherited identity; rolling back restores it.
- A partition created with `PARTITION OF` or attached with `ATTACH PARTITION`
  receives a copy of each index of its partitioned parent, as in PostgreSQL:
  an equivalent valid index it already has is attached, otherwise a new one is
  built (a unique copy fails with `23505` `could not create unique index` over
  duplicate rows), and a partitioned partition passes the copy on to its own
  partitions. So after `CREATE UNIQUE INDEX ON orders (id)`, a partition added
  later rejects a duplicate `id`. Detaching a partition keeps its copies as
  independent indexes. A foreign partition takes no copies and is refused when
  the parent has a unique index. An equivalent `UNIQUE` or `PRIMARY KEY`
  constraint of the new partition is not yet attached to a parent's plain
  unique index; pos3ql builds a separate copy. Startup refuses (`22000`) a saved
  catalog in which a valid partitioned index lacks its copy on a partition.

#### Syntax and errors

- `U&'d\0061t\+000061'` and `U&"d!0061t" UESCAPE '!'` read Unicode escapes
  as PostgreSQL does, with its errors (`invalid Unicode escape`, `invalid
  Unicode surrogate pair`, `invalid Unicode escape character`). The
  `standard_conforming_strings` setting is always on, so these forms are always
  allowed.
- An identifier longer than 63 bytes is cut to 63 bytes at a character
  boundary, and the server sends `NOTICE: identifier "..." will be truncated
  to "..."` before running the query, as PostgreSQL does.
- Errors point at their cause, as psql's `LINE 1:` and caret show: syntax
  errors, unknown relations, columns, types, functions and operators (with
  PostgreSQL's hints), failed constant input (`SELECT 'zap'::int`) and syntax
  errors inside a SQL function body given to `CREATE FUNCTION`. An error
  about a whole expression points at its start, as in PostgreSQL: the `1` of
  `SELECT 1 WHERE 1` (`argument of WHERE must be type boolean, not type
  integer`), a `CASE WHEN`, `AND`, `OR` or `NOT` operand.
- Errors and notices raised inside PL/pgSQL and SQL-language functions carry
  PostgreSQL's `CONTEXT` lines, innermost first: `PL/pgSQL function
  f(integer) line 3 at RAISE`, `SQL statement "INSERT INTO t VALUES (1)"`,
  `PL/pgSQL assignment "x := 1/0"` and `SQL function "f" statement 2`. An
  error analysing a query a function runs, such as an unknown column in
  `EXECUTE 'SELECT nocol FROM t'`, reports that query and the error's place in
  it as psql's `QUERY:` and `LINE` lines instead of the client's statement.
  `GET DIAGNOSTICS ... PG_CONTEXT` and `GET STACKED DIAGNOSTICS ...
  PG_EXCEPTION_CONTEXT` return the same text, and `RAISE;` re-raises the
  caught error with its original context. A call PostgreSQL inlines into its
  caller's query (a SQL function whose body is one `SELECT expression`) adds
  no line when its arguments vary; with constant arguments the body is folded
  while it is inlined, so `SELECT f(0)` for a body `SELECT 1/$1` reports
  `SQL function "f" during inlining`, or `SQL function "f" statement 1` when
  `f` is `IMMUTABLE` and PostgreSQL runs it while planning. A longer context than 64 KiB is reported as a `54000` error.
- Constraint violations carry PostgreSQL's `DETAIL` and object fields:
  `Failing row contains (null, x).` with the schema, table and column of a
  not-null violation; `Key (id)=(1) already exists.` with the constraint of a
  unique, foreign-key or exclusion violation; and the schema, data type and
  constraint of a domain violation. `RAISE ... USING COLUMN`, `CONSTRAINT`,
  `DATATYPE`, `TABLE` and `SCHEMA` set those fields (up to 1,024 bytes each)
  and `GET STACKED DIAGNOSTICS` reads them.
- Key and row details follow PostgreSQL's privilege rules
  (`ExecBuildSlotValueDescription`): the keys of unique, exclusion, foreign
  key and partition-routing violations (`Partition key of the failing row
  contains (id) = (21).`) show only to a user with `SELECT` on the table or
  every key column, and never under row security; otherwise a unique violation
  has no detail and the others say `Key conflicts with existing key.`, `Key is
  not present in table "p".` or `Key is still referenced from table "c".`. A
  `Failing row contains (...)` detail then names only the columns the user may
  read or the statement wrote (`(a, c) = (1, 3)`). A subscription's applied row
  is described by the applying role's privileges.
- `PREPARE` and the extended protocol's Parse convert literals as
  PostgreSQL's parse analysis does: a literal cast to a built-in type
  (`''::tsquery`, `int4 '7'`) or converted by analysis runs its input function
  once there, raising its notice or error, and the statement keeps the
  converted value in its `prepared_bytes` slot (`54000` beyond). A cast's
  length coercion and constant folding (`1/0`) run when the statement is
  planned, at each `EXECUTE`. An input notice or error carries the literal's
  cursor position when it is cast, assigned, compared or passed to a full-text
  routine.
- JSON nests as deeply as PostgreSQL 18.6's `json` and `jsonb` inputs do under
  the default `max_stack_depth` (14,521 and 14,520 levels on CI's pinned
  linux/amd64 `postgres:18` image; other builds differ, see
  [contributing](../CONTRIBUTING.md#development-and-resource-policy)); deeper documents, or a recursion that would exhaust the query
  thread's fixed stack, fail with `54001` `stack depth limit exceeded`
  (`differential_exact/747`). PostgreSQL's limit varies with each operation's
  frames, which pos3ql does not model.
- `COPY FROM` reports errors in PostgreSQL's order. PostgreSQL buffers rows
  and adds their index entries only when the buffer flushes: after 1,000 rows,
  after 64 KB of input lines, and at the end of the data. So if row 2
  duplicates row 1 and row 3 fails a `CHECK`, the `CHECK` error is reported.
  A duplicate found at a flush has the context `COPY t, line 2` with no input
  text. Rows are not buffered, and a duplicate is reported at once with its
  line text, when the table has a `BEFORE` row trigger, is a foreign table, an
  omitted column's default calls a volatile function other than `nextval`, or
  `WHERE` is volatile. `COPY FROM` into a table whose row-level security
  policies apply to the user fails with `0A000`, as in PostgreSQL.
- `COPY` into a view, materialized view or sequence is refused with `42809`
  only after the server has started reading the client's data, as PostgreSQL
  does, so `psql` reports it at the end of the data. Privileges and the column
  list are checked first. `COPY` of an index fails at once with `cannot open
  relation`. `COPY ... TO` of a view, foreign or partitioned table hints at the
  `COPY (SELECT ...) TO` form. Once a `COPY FROM` has failed, the client's
  `CopyFail` ends it with that error rather than a second one.
- An untyped string literal is converted with its target type's input routine
  while the statement is analysed, as PostgreSQL does, so it fails even when no
  row evaluates it: `SELECT nextval('missing') FROM empty_table`,
  `CASE WHEN false THEN 'abc'::integer END`, `UPDATE t SET n = 'abc' WHERE
  false`, a `COPY ... WHERE` predicate and a column default all report the bad
  input. This covers built-in types (including `regclass` and the other
  object-identifier types), arrays of them and enums; domain constraints and
  length limits are still checked when the value is computed.
- Checks PostgreSQL makes after parsing are reported when the statement would
  run, with PostgreSQL's code and message: type modifiers (`numeric(2000)` is
  `22023`, `NUMERIC precision 2000 must be between 1 and 1000`; `numeric(5,-2)`
  and `numeric(3,5)` are accepted), COPY options, storage parameters and
  sequence, database and large-object values. An earlier statement in the same
  query string still runs, and a syntax error anywhere in the string is
  reported first. In a `SELECT`, errors PostgreSQL's analysis finds earlier
  come first: `SELECT 1::numeric(5000) FROM missing` reports the missing
  relation, and `1::nosuch(5)` reports `type "nosuch" does not exist`.
- Statements are analysed before they read a row, as in PostgreSQL: a `WITH`
  query nothing reads must still be valid (`WITH x AS (SELECT * FROM missing)
  SELECT 1` fails), a constant compared with or added to a column must convert
  to its type (`WHERE id = 'abc'` fails even on an empty table, and so does a
  subquery `(SELECT id FROM t WHERE id = 'abc')` of a `SELECT`, `INSERT`,
  `UPDATE` or `DELETE`, including one in `RETURNING`), and
  `CREATE FUNCTION` checks a SQL body's queries, target relations, target
  columns and assigned value types unless `check_function_bodies` is off
  (always for a `BEGIN ATOMIC` body).
- A value written to a column must convert to the column's type with an
  implicit or assignment cast, as PostgreSQL checks before any row is read.
  An untyped literal (`'5'`) is read by the column type's input, and any
  value converts to a text column, but `INSERT INTO t (id) VALUES
  ('5'::text)` fails with `42804` (`column "id" is of type integer but
  expression is of type text`, hint `You will need to rewrite or cast the
  expression.`) pointing at the value. A subscripted target checks the
  element, or the whole array for a slice: `UPDATE t SET m[1] = 'x'::text`
  reports `subscripted assignment to "m" requires type integer but expression
  is of type text` at `m`. `INSERT ... SELECT` checks the query's output
  types, so a `UNION` of untyped literals is text. A `SELECT *` mismatch has
  no cursor position.

#### Triggers, routines and data-changing `WITH`

- PL/pgSQL converts a value to a variable's, array element's, `OUT`
  parameter's or result's type with an assignment cast when one exists, and
  otherwise through text, as PostgreSQL does: `x := true` for an integer `x`,
  or `x[1] := true` for an integer array, fails with `invalid input syntax for
  type integer: "t"`. `CREATE FUNCTION` checks that a SQL
  function's final query returns one column of a type that converts to a
  scalar result (`return type mismatch in function declared to return
  integer`, detail `Actual return type is text.`), with context `SQL function
  "f"`; an untyped literal there is text.
- A routine's `INSERT`, `UPDATE` or `DELETE` on a view or on a table with
  rules goes through the view's triggers and the rules, as at top level:
  static PL/pgSQL, `EXECUTE` and SQL-language functions alike. A static
  statement reads the routine's variables and a trigger's `NEW` and `OLD`
  rows, so `INSERT INTO active_orders VALUES (id)` works in a body, also in a
  `BEFORE` row trigger and in a trigger of a write that another trigger made.
- A `BEFORE` or `INSTEAD OF` row trigger whose body runs DDL (`CREATE TABLE`,
  `EXECUTE 'CREATE ...'`), writes a view or a table with rules, calls a
  procedure that does, or calls a function that reads the trigger's own
  earlier writes, runs once for its row as in PostgreSQL: the write stops at
  that row, the whole trigger body runs, and the write continues with the
  row the body returned. Its notices, sequence values and writes appear once.
- A trigger function may `RETURN` any row-valued expression, as in
  PostgreSQL: `RETURN coalesce(NEW, OLD)` serves INSERT, UPDATE and DELETE
  alike, and NULL skips the row in a `BEFORE` trigger. A `BEFORE INSERT` or
  `UPDATE` trigger's row becomes the written row. A row whose column count or
  types (by position) differ from the table's fails with `42804` (`returned
  row structure does not match the structure of the triggering table`) at
  function exit, and a non-row value fails at the `RETURN`. A whole-row
  reference such as `NEW` or `t` keeps each column's declared domain, enum or
  composite type.
- A superuser-only parameter such as `session_replication_role` or
  `track_functions` needs the privilege to set it (superuser, or `GRANT SET
  ON PARAMETER`) everywhere PostgreSQL checks: `SET`, `RESET`, `set_config`,
  connection options, a routine's `SET` clause at `CREATE` and `ALTER` and
  again at every call (the owner's privilege under `SECURITY DEFINER`, the
  caller's otherwise), and `ALTER ROLE ... SET` / `ALTER DATABASE ... SET`.
  Each fails with `42501` (`permission denied to set parameter "..."`), as
  does `ALTER SYSTEM` without `ALTER SYSTEM` on the parameter. A revoked
  grant stops calls of a function whose `SET` clause needed it.
- The built-in `suppress_redundant_updates_trigger()` skips an `UPDATE`
  whose new row is stored identically to the old one, as in PostgreSQL:
  `CREATE TRIGGER z BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION
  suppress_redundant_updates_trigger()` makes `UPDATE t SET a = a` report
  `UPDATE 0`. Values compare as stored, so `1.5` and `1.50` differ. Any other
  event, timing or level fails with `39P01` when it fires.
- `session_replication_role` (superusers, or roles granted `SET` on it)
  chooses which triggers and rules fire, as in PostgreSQL: `replica` fires
  those enabled `REPLICA` or `ALWAYS`; `origin` (the default) and `local`
  fire those enabled `ORIGIN` or `ALWAYS`. Foreign key checks and actions are
  `ORIGIN` triggers, so `SET session_replication_role = replica` loads rows
  without them, for example an order line before its order. A superuser's
  `ALTER TABLE orders DISABLE TRIGGER ALL` (as `pg_dump --disable-triggers`
  emits) also stops the foreign keys' `RI_ConstraintTrigger_*` checks on a
  referencing table or actions on a referenced one; other roles get `42501`
  (`permission denied: "RI_ConstraintTrigger_c_..." is a system trigger`),
  and `USER` leaves them alone. A view's
  `INSTEAD OF` trigger that does not fire leaves the row unwritten without an
  error.
- Writing through an automatically updatable view into a view below it that
  has its own `INSTEAD OF` trigger or rewrite rule for the write writes that
  view, as PostgreSQL's rewriter does: with `v2` selecting `a AS x, b * 2 AS
  b2` from trigger view `v1`, `INSERT INTO v2 (x) VALUES (3) RETURNING *`
  fires `v1`'s trigger with `(3, NULL)` and returns the row it returns, and
  the check options of the views above apply to that row. Their column
  defaults reach `v1`'s trigger and rules' `NEW`. A conditional `DO INSTEAD`
  rule without an unconditional one or a trigger fails with `55000` (`Views
  with conditional DO INSTEAD rules are not automatically updatable.`), and
  `MERGE` into a relation with rules fails with `0A000`.
- A trigger's `NEW` and `OLD` have the table's row type in every firing. One
  without a row (`OLD` in an `INSERT` trigger, `NEW` in a `DELETE` trigger,
  both in a statement trigger) is NULL, and so are its fields; a field the
  table lacks fails with `42703` (`record "new" has no field "x"`).
- An exception handler accepts every PostgreSQL condition name, such as
  `duplicate_table` or `invalid_text_representation`; a name that belongs to
  two codes (`null_value_not_allowed`) catches both. An unknown name fails with
  `42704` (`unrecognized exception condition "x"`).
- A `WITH` query of a static PL/pgSQL statement's main query cannot read
  the routine's variables: `INSERT INTO t WITH c AS (SELECT id_var) SELECT *
  FROM c` fails with `42703` (`column "id_var" does not exist`), where
  PostgreSQL reads the variable. Move the query into a derived table
  (`SELECT * FROM (SELECT id_var) c`), which reads routine variables.
- A rule on a data-modifying `WITH` sub-statement's target must leave one
  statement, as in PostgreSQL: `DO ALSO`, conditional, multi-statement,
  `NOTHING` and `NOTIFY` `INSTEAD` rules fail with `0A000` before anything
  runs. The command tag of a statement an unconditional `INSTEAD` rule
  replaces counts the last `INSTEAD` action of the same command.
- `INSERT`, `UPDATE`, `DELETE` and `MERGE ... RETURNING ... INTO [STRICT]
  targets` in PL/pgSQL store the returned row and set `FOUND` and
  `ROW_COUNT`. A routine called in the statement's rows or `RETURNING` list
  sees the rows the statement changed before it, as at top level. More than
  one row fails with `P0003` (`query returned more than one row`) even without
  `STRICT`; no row fails with `P0002` only under `STRICT` and otherwise stores
  NULLs. `RETURNING` without `INTO` fails with `query has no destination for
  result data`, and `INTO` without `RETURNING` with `INTO used with a command
  that cannot return data`. As in PostgreSQL, extra result columns are
  ignored and targets without a column become NULL.
- `FOR target IN INSERT | UPDATE | DELETE | MERGE ... RETURNING ... LOOP`
  and the same through `FOR ... IN EXECUTE` run the statement to completion
  before the first iteration, as PostgreSQL does, so the loop body sees
  every changed row. Without `RETURNING` the loop fails with `42P11`
  (`cannot open INSERT query as cursor`). A scalar target takes the first
  returned column. For example, `FOR r IN DELETE FROM jobs WHERE done
  RETURNING id LOOP ... END LOOP` visits each deleted job once.
- A function's name qualifies its parameters, as in PostgreSQL: in
  `CREATE FUNCTION f(a int)`, `f.a` is the parameter `a` in SQL and
  PL/pgSQL bodies, even when a table in the query has a column `a`.
- PL/pgSQL blocks nest with their own `DECLARE` sections, and a label
  (`<<outer>>`) names a block or loop as in PostgreSQL. `outer.x` reads or
  assigns the `x` that block declares even where an inner block or an integer
  `FOR` counter shadows it; `EXIT outer` leaves a labelled block or loop and
  `CONTINUE outer` resumes a labelled loop. A nested block's variables are
  initialized in order each time it is entered, outside its own exception
  handlers. `END outer` and `END LOOP outer` must repeat the label (`end label
  "b" differs from block's label "a"`).
- A routine's parameters are PL/pgSQL variables under their names and as
  `$1`, `$2`, ... in declaration order, output parameters included, so
  `$1` sees an assignment made through the parameter's name and `$2 := 'x'`
  sets an unnamed `OUT` parameter. `a ALIAS FOR $1` (or for a parameter's
  name, an earlier variable, `label.x`, or a trigger's `NEW` and `OLD`)
  declares another name for the same variable: `a := a + 1` changes `$1`.
  An alias of a name that is not declared fails when the routine is created
  (`variable "$2" does not exist`).
- `FOR a, b IN SELECT ...` (or `EXECUTE`, or a data change with
  `RETURNING`) assigns each row's columns to the listed scalar variables in
  order: missing columns are NULL, extra ones ignored, and a loop without
  rows leaves them all NULL. A single scalar variable takes the first column.
  `strict_multi_assignment` reports a count mismatch. Integer and cursor
  loops take one variable (`integer FOR loop must have only one target
  variable`), and a record in the list fails (`"r" is not a scalar
  variable`).
- A name in a PL/pgSQL statement that is both a variable and a column is
  ambiguous, as in PostgreSQL: with parameter `a`, `UPDATE t SET b = 1/(a-15)`
  fails with `42702` (`column reference "a" is ambiguous`, detail `It could
  refer to either a PL/pgSQL variable or a table column.`) before any row is
  read, at any query level, including a cursor's query at `OPEN`.
  `#variable_conflict use_column` at the start of the body, or the superuser
  setting `plpgsql.variable_conflict = use_column`, reads the column instead;
  `use_variable` reads the variable everywhere, so with variable `a = 2`,
  `SELECT sum(a) FROM t` adds 2 per row and `GROUP BY a` makes one group.
- PL/pgSQL "loads" in a session the first time the session runs or creates
  PL/pgSQL code, as PostgreSQL loads its library. Until then its settings
  (`plpgsql.variable_conflict`, `plpgsql.print_strict_params`,
  `plpgsql.check_asserts`, `plpgsql.extra_warnings`, `plpgsql.extra_errors`)
  are unknown: `SHOW plpgsql.variable_conflict` fails with `42704`, `SET`
  stores any value as a placeholder and `pg_settings` lists none of them.
  Loading turns each placeholder into the setting at every transaction
  level; a value the setting rejects (`SET plpgsql.variable_conflict =
  'bogus'`), or that the role could not set, is dropped with PostgreSQL's
  warning (`invalid value for parameter ...` with its hint). Other
  placeholders named `plpgsql.anything` are then removed with `WARNING:
  invalid configuration parameter name "plpgsql.foo", removing it`, and a
  later `SET plpgsql.bar = 'y'` fails with `42602` (`"plpgsql" is a reserved
  prefix.`). A rejected value warns once for the session's own `SET` and
  once for each transaction level (`SAVEPOINT`) that assigned it, even to the
  same value. `ALTER SYSTEM` changes no session: before loading it stores any
  `plpgsql.*` value unchecked, and `ALTER SYSTEM RESET plpgsql.foo` (like
  `ALTER ROLE ... RESET`) removes a setting under the reserved prefix.
  `plpgsql.check_asserts = off` skips `ASSERT` statements.
- PL/pgSQL's extra checks follow `plpgsql.extra_warnings` (a `WARNING`) and
  `plpgsql.extra_errors` (an `ERROR`, which wins), each `none`, `all` or a
  list such as `too_many_rows, strict_multi_assignment`.
  `shadowed_variables` reports a declaration that hides a parameter, `found`,
  a trigger variable, a loop variable or an enclosing block's variable, when
  `CREATE FUNCTION` or `CREATE PROCEDURE` checks the body. `too_many_rows`
  reports a `SELECT ... INTO` without `STRICT` that finds a second row (its
  `ROW_COUNT` is then 2). `strict_multi_assignment` reports a row stored into
  a different number of variables or record fields, once per missing source
  and once for extra columns, as `SELECT 1, 2 INTO a` does.
- A PL/pgSQL function keeps the `plpgsql.variable_conflict` and
  `plpgsql.print_strict_params` values it was compiled under, as PostgreSQL's
  per-session function cache does: the creating session compiles it at
  `CREATE FUNCTION` for calls without collatable inputs, and other calls
  compile it at their session's first call. A body's `#variable_conflict` or
  `#print_strict_params` option overrides the setting; `#option dump` is
  accepted. With `print_strict_params` on, a `STRICT` failure's detail lists
  the variables the statement reads, in declaration order (`parameters: p =
  '1', q = NULL`), or an `EXECUTE`'s `USING` values as `$1 = '1'`.
- Each query of a volatile PL/pgSQL function, procedure, trigger or `DO` block
  sees the rows its earlier statements changed; a stable or immutable
  function reads its caller's snapshot.
- Data-changing `WITH` queries run exactly once and share the main query's
  snapshot, as in PostgreSQL: in `WITH d AS (DELETE FROM t) INSERT INTO t
  VALUES (9)`, the delete removes only the rows that existed before the
  statement, and the main query sees a `WITH` query's changes only through
  its `RETURNING` rows. Each runs when the statement first reads its rows,
  so in `WITH w AS (INSERT INTO t VALUES (1) RETURNING a) SELECT f(), (SELECT
  a FROM w), f()` the first call of a volatile `f` does not see the new row
  and the second does. One that nothing reads runs after the main query, the
  last written first.
- A subquery (scalar, `EXISTS`, `ARRAY`, `IN`, `ANY` or `ALL`) that calls a
  volatile function or reads a `WITH` query not yet run, or any subquery of a
  statement with a data-changing `WITH`, runs where evaluation reaches it,
  once when it is uncorrelated, so its notices, sequence values and writes
  interleave with the rest of the expression as in PostgreSQL; an untaken
  `CASE` arm never runs it. Such an `IN`, `ANY` or `ALL` list is held in
  statement memory rather than spilled.
- A subquery or derived table reads the columns of every query that encloses
  it, however deeply nested: in `SELECT (SELECT (SELECT a.x + b.y FROM c)
  FROM b) FROM a` the innermost query reads both `a` and `b`. A derived table
  in FROM reads the FROM items before it only when marked `LATERAL`; `SELECT
  * FROM a, (SELECT * FROM b WHERE b.y = a.x) s` fails with PostgreSQL's
  `invalid reference to FROM-clause entry for table "a"` and its hint to mark
  the subquery `LATERAL`. `UPDATE ... FROM` and `DELETE ... USING` items
  cannot read the target table, even when `LATERAL`.
- A materialized `WITH` query (or one read more than once) runs only as far
  as its readers read it, as PostgreSQL's CTE scan pulls rows into the store
  its readers share; one that nothing reads never runs. For example, `WITH c
  AS MATERIALIZED (SELECT 1 / (a - 2000) FROM t) SELECT * FROM c LIMIT 2`
  returns two rows without reaching the row that divides by zero. A reader
  that needs rows past where an earlier read stopped runs the query again
  from its first row; a query calling a volatile function would evaluate
  those rows differently, so that read fails with `0A000`. Readers that
  are the only item of their `FROM` read this way; a join reads the whole
  query at its first read.
- `UPDATE` and `INSERT` assign through composite fields and array elements:
  `UPDATE t SET pair.x = 1, pair.y = 'a'`, `SET n.inner.x = 3`, `SET
  a[2].x = 5`, `SET bag.arr[1:2] = '{1,2}'` and `INSERT INTO t (pair.x,
  pair.y) VALUES (1, 'a')`. A NULL composite becomes a row of NULL fields
  first. Several targets through one column apply left to right; any other
  repeated target fails (`multiple assignments to same column "pair"`, or
  `column "pair" specified more than once` in an `INSERT`). Values convert
  as assignments do, and PostgreSQL's analysis errors are reported at the
  target (`subfield "x" is of type integer but expression is of type
  timestamp with time zone`, `cannot set a subfield to DEFAULT`).
- A PL/pgSQL condition of another type converts through its text form as in
  PostgreSQL: `IF 1` is true and `IF 5` fails with `invalid input syntax for
  type boolean`.
- An unsupported cast reports `42846` with both types as `format_type` names
  them (`cannot cast type integer to timestamp without time zone`).
- Array elements and slices can be assigned in `UPDATE`, `INSERT` and
  PL/pgSQL (`UPDATE t SET a[2] = 9`, `a[1:2] = '{1,2}'`, `INSERT INTO t (a[1])
  VALUES (5)`, `x[2][1] := 8`, `r.f[1] := 3`; `differential_exact/523`): a
  one-dimensional array grows to the subscript and fills the gap with NULLs, a
  NULL array becomes `[5:5]={1}`, a multidimensional array fails with `2202E`
  outside its bounds, and a slice anywhere in a subscript list makes the whole
  list a slice (`a[1:1][1]`). Further indirection after a slice (`a[1:2].x`)
  fails with `0A000`. A subscripted jsonb target in an `INSERT` starts from a
  jsonb NULL (`INSERT INTO t (j['x']) VALUES ('1')` stores `{"x": 1}`).

#### Row types

- Every table, partitioned table, foreign table, materialized view and view has
  a row type named like the relation. `ROW(1, 'a')::orders`, `'(1,a)'::orders`,
  `orders[]`, whole-row references (`SELECT o FROM orders o` has type
  `orders`, as do `orders.*` and `orders` in `UPDATE`/`DELETE` conditions and
  `RETURNING`), routine parameters and results (`RETURNS SETOF orders`) and table
  columns (`CREATE TABLE archive(o orders)`) and prepared statement
  parameters (`PREPARE q(orders) AS SELECT $1.note`) behave as in PostgreSQL. A routine
  or column using the row type blocks `DROP TABLE orders`; `CASCADE` drops the
  routine or column. Comparing two composite values uses PostgreSQL's record
  ordering, so `ROW(1, NULL)::orders = ROW(1, NULL)::orders` is true, and a
  row `IS NULL`/`IS NOT NULL` test reads every field: an `orders` row with
  one NULL column is neither.
- `CHECK` constraints, index keys and index predicates may use the whole row
  (`CHECK (orders IS NOT NULL)`, `CREATE INDEX ON orders (total_of(orders.*))`)
  with PostgreSQL's constraint names, `conkey`, `pg_depend` rows and printed
  definitions; `(orders).total` and `orders.total` mean the column itself.
  Copying such a constraint or index with `LIKE`, `INHERITS` or `PARTITION OF`
  fails with PostgreSQL's `0A000` `cannot convert whole-row table reference`,
  and an extended statistics expression cannot use it. Renaming the table or
  a column rewrites the stored keys and predicates, as PostgreSQL's printed
  definitions show. An index key that is the row itself
  (`CREATE INDEX ON orders ((orders))`) fails with `0A000`.
- Renaming or moving a function or type, or renaming an enum value, fails
  with `0A000` while a column default or generated expression, `CHECK` or
  exclusion predicate, index key or predicate, statistics expression, trigger
  `WHEN` condition or domain constraint names it, because those are stored
  as SQL text; for example, after
  `CREATE TABLE t (a int CHECK (f(a) > 0))`, `ALTER FUNCTION f(int) RENAME TO g`
  is refused. Views, rules, policies and SQL-standard routine bodies follow
  the rename, and so do enum literals that a domain constraint or an enum
  column's default binds. After `ALTER FUNCTION s.f(int) RENAME TO g`, a
  function `RETURN s.f(1)` still calls it and prints as `RETURN s.g(1)`.
- Index keys and predicates must be immutable (`42P17` `functions in index
  expression must be marked IMMUTABLE`), judged as PostgreSQL does after
  inlining simple SQL functions: `CREATE INDEX ON orders ((placed_at::date))`
  is refused for a `timestamptz` column because that cast depends on the
  `TimeZone` setting, and so is a `STABLE` PL/pgSQL function, while a
  `VOLATILE` SQL function whose body is `SELECT $1` is accepted.
- A view's row type has the view's column types, modifiers and collations as
  they were when the view was created or replaced: `CREATE VIEW totals AS
  SELECT sum(amount) AS total FROM orders` keeps `total numeric` even if a later
  query would describe it differently. `CREATE OR REPLACE VIEW` may only add
  columns, as in PostgreSQL (`cannot change data type of view column "total"
  from numeric to integer`).
- While a column uses the row type, `ALTER TABLE orders ADD COLUMN ... DEFAULT`
  and `ALTER COLUMN ... TYPE` fail with `0A000` as in PostgreSQL, and `ALTER
  COLUMN ... TYPE` also fails while a view, rule, policy or SQL-standard routine
  body reads the column. Every drop with `CASCADE` that removes a relation,
  including the cascade of another object (`DROP FUNCTION f CASCADE` where
  view `v` calls `f`, `DROP SCHEMA`, `DROP OWNED`), also drops other tables'
  columns, composite type attributes (`CREATE TYPE wrap AS (o orders)`) and
  domains of the relation's row type, as PostgreSQL does.

#### Functions and expressions

- Column defaults, CHECK constraints, parameter defaults, `RETURN` bodies,
  index predicates, trigger `WHEN` and rule conditions print in PostgreSQL's
  analyzed form (`pg_get_expr`, `pg_get_constraintdef`,
  `pg_get_function_arguments`, `pg_get_functiondef`) for literals, columns,
  parameters, operators, `IN`, `LIKE`, null and boolean tests, `IS [NOT]
  DISTINCT FROM`, `CASE`, casts, SQL value functions, positional calls,
  `ROW(...)` and whole-row references; other expressions print as stored.
  `DEFAULT 'x'` on a `text` column shows as `'x'::text`, and `CHECK (v <> '')`
  on a `varchar` column as `CHECK (((v)::text <> ''::text))`; pretty output
  (`pg_get_constraintdef(oid, true)`, as psql's `\d` uses) drops redundant
  parentheses. `pg_get_viewdef` and `pg_get_ruledef` deparse the stored query,
  never its text, with PostgreSQL's indented layout, wrap column and
  qualification for views over tables, other views, subqueries, functions,
  joins, set operations, `WITH` queries, `LATERAL` items and `VALUES` lists,
  naming a table read again in a nested query `t_1` and printing the current
  names of renamed tables, functions, types and enum values; a shape the
  deparser does not cover fails with `0A000` (`deparsing the query of view
  "v" is not supported`). In a
  `BEGIN ATOMIC` body only a `SELECT` of expressions without `FROM` is
  deparsed; other statements print as written. A `FROM` function returning
  `record` with a column definition list is accepted only for the
  `json[b]_to_record[set]` family.
- A SQL function whose body is one expression is inlined as PostgreSQL's
  `inline_function` does; [query planning](query-planning.md) gives the rules.
- An index key or predicate may call a PL/pgSQL function
  (`differential_exact/755`): a utility statement (`CREATE INDEX`, `REINDEX`,
  `CLUSTER`, `VACUUM`, `ANALYZE`, `ALTER TABLE`, `CREATE TABLE AS`,
  materialized view population) runs such calls and restarts replaying their
  results; a function's notice is raised once per row and index (`833`);
  `REINDEX TABLE` builds indexes one at a time in OID order. An index whose
  expression no longer resolves after `ALTER COLUMN ... TYPE` fails with
  `function rw(bigint) does not exist` and its hint (`863`).
- Collations: conflicting explicit `COLLATE` clauses fail at analysis
  (`assign_collations`, `832`); an unknown collation (`42704`, or `3F000` for a
  missing schema) and a `COLLATE` on a type without collation (`42804`) fail at
  the clause with its cursor (`860`, `861`). A domain has a collation
  (`pg_type.typcollation`): its own `COLLATE`, else its parent's or base
  type's; columns and attributes of the domain take it unless they write their
  own, and a custom collation it uses is a dependency (`864`). `DROP TYPE`
  drops a domain as `DROP DOMAIN` does (`865`).
- `ALTER COLUMN ... TYPE` to a domain, enum or composite rewrites through the
  type: a domain's checks apply to each row, an enum or composite needs `USING`
  unless the column already has the type (`42804` with PostgreSQL's hint), and
  a default that cannot convert is refused (`866`).
- Constraints: table-level `NOT NULL` constraints merge with column ones as in
  PostgreSQL 18; a `NOT VALID` not-null constraint checks new rows until
  validated (`745`); a CHECK merges with a same-named inherited one when their
  analyzed predicates are equal, with PostgreSQL's conflicts otherwise (`742`);
  a column merged with an inherited one at another position is reported as
  moved (`781`). A unique btree build sorts keys with PostgreSQL's quicksort
  in physical order and names the first equal pair PostgreSQL compares; a
  build whose keys outgrow the statement arena fails with `54000`.
- A table's or materialized view's `fillfactor` and `autovacuum_enabled` keep
  the order they were set in (`927`); pos3ql runs no autovacuum, so either
  value holds. Other heap storage parameters fail with `22023`.
- A `DROP TABLE ... CASCADE` drops every referencing foreign key in one
  statement (`898`); a transaction that changes one relation in more than
  `max_catalog_versions_per_object` statements fails with that named capacity
  error.
- PL/pgSQL row and record variables (`NEW` and `OLD` included) are visible at
  every query level: `(r).*`, `r.*` and `ROW(r.*)` expand them; a record
  holding an anonymous record or none fails with `42809`, and `r.*` naming
  both a FROM item and a variable follows `plpgsql.variable_conflict` (`780`);
  a cursor may be declared in a nested block (`743`, `744`). A PL/pgSQL `FOR`
  loop names its query in an error's context only when planning raised it
  (`944`).

- Named arguments and omitted defaults run in declared input order and share
  the caller's session, so a default calling `nextval('order_numbers')` updates
  `currval`. Function lookup never runs arguments to discover their types.
- In `FROM`, named `OUT`/`INOUT` parameters become columns
  (`SELECT total FROM calculate_order(42)`). The 100-input-argument limit does
  not limit result width: `SELECT c0, c100 FROM wide_function_rows()` works.
- Stored defaults keep the objects and overloads chosen at creation, even after
  renames, schema moves, or new overloads; `nextval('order_numbers'::text)`
  looks the name up at each call.
- A SQL function or procedure with a result must end with a `SELECT` or a
  data-modifying statement with `RETURNING` whose columns fit the declared
  result. Otherwise `CREATE` fails with `42P13` (`return type mismatch in
  function declared to return integer`) and PostgreSQL's detail, such as
  `Final statement must return exactly one column.` for `RETURNS integer AS
  'SELECT 1, 2'` or `Actual return type is text.` for `AS 'SELECT ''a''::text'`.
  Conversions allowed in assignment pass (`SELECT 1::bigint` for `integer`).
  With `check_function_bodies = off`, or with polymorphic arguments, the check
  runs at the call and names the resolved type (`declared to return boolean`).
  Procedure output parameters take the final statement's first row.
  Transaction control in a quoted SQL body (`AS 'COMMIT'`) is stored, and a
  call that reaches it fails with `0A000`, as in PostgreSQL. A `BEGIN ATOMIC`
  body accepts only queries and data changes; anything else, `CALL` included,
  fails at `CREATE` (`0A000`, `COMMIT is not yet supported in unquoted SQL
  function body`). A quoted SQL body cannot `CALL` a procedure with output
  parameters (`0A000`, `calling procedures with output arguments is not
  supported in SQL functions`). `pg_get_function_sqlbody`
  returns a `RETURN` or `BEGIN ATOMIC` body (`RETURN (x * 2)`; `BEGIN ATOMIC`
  then one statement per line) and NULL for a quoted body; a `SELECT` of
  expressions without `FROM` is printed in PostgreSQL's analyzed form with
  each column's name (`SELECT (x + 1);`, `SELECT abs(x) AS abs;`), other
  statements as written.
- PL/pgSQL `CALL` assigns output parameters to the variables written as
  their arguments: `CALL doubled(x, y)` updates `x` and `y`, and a constant in
  an output position fails with `42601` (`procedure parameter "b" is an output
  parameter but corresponding argument is not writable`). `EXECUTE 'CALL ...'`
  returns the outputs only through `INTO`. A called procedure may `COMMIT`
  when every caller up to the top-level `CALL` or `DO` could; from a function,
  an explicit transaction block, an exception block, or a `SECURITY DEFINER`
  or `SET` procedure it fails with `2D000` (`invalid transaction
  termination`). A trigger function may `CALL` a procedure; the call is
  atomic, so its `COMMIT` fails with `2D000`. A trigger fired inside a write
  (a BEFORE row trigger, or a deferred constraint trigger at `COMMIT`) can
  call PL/pgSQL and SQL-language procedures, with arguments such as
  `NEW.id + 1`. A SQL procedure that a deferred constraint trigger calls at
  `COMMIT` runs its queries, `INSERT`, `UPDATE`, `DELETE` and nested `CALL`
  statements; any other statement in its body, such as `CREATE TABLE`, fails
  with `0A000` (`a SQL procedure called during a write cannot run CREATE
  TABLE`) where PostgreSQL runs it. From a `BEFORE` row trigger it runs.
- A procedure or `DO` block that waits for a lock or storage, before or after
  its own `COMMIT`, continues at the waiting statement as in PostgreSQL. A
  PL/pgSQL statement whose function call waits continues at that call,
  including statements with several expressions (`FOR` bounds, `CASE`,
  `RAISE` arguments and `USING` options, `IF ... ELSIF`, `ASSERT`, `CALL`
  arguments, `EXECUTE` command and `USING` values): expressions that finished
  before the wait are not evaluated again. For example, in
  `FOR i IN next_batch()..last_batch() LOOP`, `next_batch()` runs once even
  when `last_batch()` waits for a lock. A dynamic `EXECUTE` that writes rolls
  back to its own start and evaluates its expressions again.
- A built-in function cannot implement `CREATE CAST` or `CREATE OPERATOR`
  (`0A000`). Aggregate support functions can name supported built-ins.
- A missing-function message names the whole signature however long it is,
  as PostgreSQL does, including the signature that `DROP`, `ALTER`,
  `COMMENT ON` and `GRANT` quote for a routine they cannot find, such as
  `function f(integer) does not exist`. Messages up to 100 arguments of
  schema-qualified type names fit; a longer one fails with `54000`.
- Each database has its own copy of a built-in function's catalog row, as in
  PostgreSQL. `ALTER FUNCTION lower(text) STABLE` changes volatility,
  `PARALLEL`, `LEAKPROOF`, `COST`, `ROWS`, `STRICT`, `SECURITY`, `SET`/`RESET`
  settings (`proconfig`, at most 16 per routine, then `54000`) and `OWNER TO`
  in the current database only; `pg_proc` shows the change, and a
  generated column or publication row filter then rejects a function marked
  stable or volatile. As in PostgreSQL, a built-in still runs with its own
  null handling and as the caller: `STRICT` only makes a constant NULL input
  fold to NULL (`array_append(ARRAY[1], NULL)` is NULL, but a NULL column
  value is still appended), and `SECURITY DEFINER` and `SET` settings change
  nothing at run time, as PostgreSQL calls built-ins without consulting
  `pg_proc`. `REVOKE EXECUTE ON FUNCTION md5(text) FROM PUBLIC` makes a later
  `SELECT md5('a')` fail with `42501` for roles without a grant. The check
  also covers the function behind an operator, cast, `IN`, `ANY`, `LIKE`,
  regular-expression match, simple `CASE`, `IS DISTINCT FROM`, `NULLIF`,
  `DISTINCT`, `GROUP BY`, window `PARTITION BY` and `ORDER BY` keys, `ROW(a,
  b) = ROW(c, d)` field comparisons and `UNION`/`INTERSECT`/`EXCEPT`: after
  `REVOKE EXECUTE ON FUNCTION int4pl(integer, integer) FROM PUBLIC`,
  `SELECT a + 1 FROM t` fails even when `t` is empty, as PostgreSQL checks
  every function when a plan starts. User-defined functions are checked the
  same way, in select lists, filters, ordering, `LIMIT`, subqueries, `INSERT`
  values and `ON CONFLICT DO UPDATE`, `MERGE` conditions and actions and
  `RETURNING`; branches the planner folds away (`WHERE false AND f(x)`,
  `CASE WHEN false THEN f(x) END`) are not checked. Every PostgreSQL 18.6
  `pg_proc` row is listed, so operator, cast and support functions such as
  `texteq`, `int4um` and `int4in` can be granted, revoked and altered; called
  by name, an operator, cast or type-input function runs as that operator or
  cast (`texteq('a', 'b')` is `'a' = 'b'`), and another listed routine
  pos3ql does not run fails with `0A000`. Server administration functions
  keep initdb's ACL: `pg_reload_conf()` and `pg_read_file(text)` are
  executable only by superusers and roles granted `EXECUTE`, and initdb's
  grants to predefined roles (`pg_monitor` may run `pg_ls_waldir()`) are
  kept in `proacl` and `pg_init_privs`. `GRANT`, `has_function_privilege`,
  `proacl`, `pg_get_function_arguments`, `pg_get_function_result` and psql's
  `\df+` follow PostgreSQL, including the description PostgreSQL ships for
  each built-in routine
  (`obj_description('lower(text)'::regprocedure)` is `lowercase`);
  `COMMENT ON FUNCTION` replaces it and `COMMENT ... IS NULL` removes it in
  the current database. Built-in schemas, languages, access methods, types,
  operators and text search objects carry their shipped descriptions the same
  way (`obj_description('public'::regnamespace, 'pg_namespace')` is
  `standard public schema`), and so does the `plpgsql` extension that initdb
  installs: it is listed in `pg_extension`, owns its language and handlers in
  `pg_depend`, and psql's `\dx` and `\dx+` show it. `CREATE EXTENSION plpgsql`
  reports that it already exists; altering, dropping or commenting on it fails
  with `0A000`. `pg_available_extensions()` and
  `pg_available_extension_versions()` run as table functions, and
  `pg_relation_size`, `pg_total_relation_size` and `pg_indexes_size` report the
  bytes of a relation's stored rows (indexes and sequences have none). Changes survive restart and
  are copied by `CREATE DATABASE ... TEMPLATE`. Each changed routine uses one
  of the `max_builtin_routine_entries` rows (default 64); running out fails
  with `54000`. A signature may name a pseudo-type, as in PostgreSQL:
  `'int4in(cstring)'::regprocedure`, `GRANT EXECUTE ON FUNCTION
  int4in(cstring) TO r` and `pg_identify_object` of `json_in(cstring)` work,
  and `'"any"'::regtype` or `'_cstring'::regtype` name their catalog types.
  A user-defined routine refuses pseudo-type arguments and results with
  PostgreSQL's messages (`SQL functions cannot have arguments of type
  cstring`). A PL/pgSQL routine may take a `record` argument
  (`CREATE FUNCTION f(r record) ... RETURN r.a`). `'abc'::cstring`,
  `name_column::cstring` and `'abc'::unknown` are values of those types, as in
  PostgreSQL, and a cast to `"any"` keeps its operand; other sources fail
  with `42846` (`cannot cast type integer to cstring`). A `cstring` only
  concatenates with text (`'a'::cstring || 'b'` is `ab`) and with `cstring[]`;
  comparing or matching it (`'a'::cstring = 'a'`), sorting, grouping or
  removing duplicates over it (`SELECT DISTINCT`, `ORDER BY`, `count(DISTINCT
  ...)`), `IN` lists and `GREATEST`/`LEAST` fail with PostgreSQL's `42883`
  messages. `cstring[]` values (`'{a,b}'::cstring[]`, `ARRAY['a'::cstring]`,
  `'{a}'::_cstring`) work with the array functions that need no element
  comparison, and a SQL routine may take or return one; a table or view
  column of either type fails with `42P16`. `x IN (SELECT c ...)` and `x op
  ANY (SELECT c ...)` resolve their operator from the subquery column's
  declared type before reading a row, so a `cstring` side fails with `42883`
  even over an empty result. The type-modifier input functions
  (`numerictypmodin('{10,2}')` is `655366`) encode and validate modifiers as
  PostgreSQL does; a temporal precision above 6 (`timetypmodin('{7}')`) is
  clamped to 6 with PostgreSQL's `WARNING`. Limits: renaming or moving a
  built-in fails with `0A000`.
  Dropping a built-in fails with `2BP01`, as in PostgreSQL.
- PostgreSQL 18.6's predefined roles exist from the start, with their
  OIDs, and initdb's memberships of `pg_monitor` in `pg_read_all_settings`,
  `pg_read_all_stats` and `pg_stat_scan_tables`. Like PostgreSQL, pos3ql
  rejects creating, altering or renaming a role whose name starts with `pg_`
  (`42939`) and dropping a predefined role (`2BP01`); `pg_database_owner` has
  only its implicit member, the current database's owner. Their privileges
  apply where pos3ql has the feature: `pg_read_all_data` reads every table,
  view and sequence and uses every schema, `pg_write_all_data` inserts,
  updates and deletes everywhere, `pg_maintain` may `VACUUM`, `ANALYZE`,
  `REINDEX`, `REFRESH MATERIALIZED VIEW` and `LOCK` every table,
  `pg_checkpoint` may run `CHECKPOINT`, `pg_signal_backend` may cancel or
  terminate other non-superuser sessions, and `pg_read_all_stats` sees every
  session's `pg_stat_activity` details. For example, after
  `GRANT pg_read_all_data TO reporting`, the `reporting` role can
  `SELECT` from any table without per-table grants. pos3ql has no server
  files, superuser-only settings, reserved connections or autovacuum workers,
  so `pg_read_server_files`, `pg_write_server_files`,
  `pg_execute_server_program`, `pg_read_all_settings`,
  `pg_use_reserved_connections` and `pg_signal_autovacuum_worker` grant
  nothing more. `VACUUM` and `ANALYZE` skip a table the role may not
  maintain with PostgreSQL's `permission denied to analyze "t", skipping it`
  warning. The predefined roles are not counted in `max_roles`.
- `DROP ROLE` of a role that still owns objects or holds privileges fails
  with `2BP01` and PostgreSQL's `DETAIL` list (`owner of table t`,
  `privileges for function upper(text)`, `privileges for column b of table
  t`, `target of policy p on table t`, `owner of default privileges on new
  relations belonging to role r`), at most 100 lines and then `and N other
  objects`, then `N objects in database d` for each other database.
  `pg_shdepend` holds the same rows for every database (with that
  database's OID in `dbid`) and for databases themselves: owners other than the bootstrap superuser and
  predefined roles, every grantee or grantor in an
  ACL other than the owner, the roles a policy names, and default-privilege
  owners and grantees. `pg_describe_object` names objects as PostgreSQL does,
  without their schema when the search path finds them (`table t`,
  `function lower(text)`, `operator <<<(integer,integer)`, `collation c`,
  `constraint c on table t`, `trigger tg on table t`, `default value for
  column a of table t`, `default privileges on new relations belonging to
  role r`), and `pg_identify_object` names default-privilege rows
  (`for role r in schema s on sequences`). Limits: owners are listed in object OID order and
  privileges in grant order, where PostgreSQL lists catalog insertion order.
  Casts, operator classes and operator families, built-in ones included,
  are described as `cast from money to numeric` and `operator class int4_ops
  for access method btree`, and `pg_cast` lists PostgreSQL's 235 built-in
  casts.
- Array and `CASE` result types follow declarations, including subqueries and
  nulls: `ARRAY[(SELECT NULL::date)]` is `date[]`; mixing integer and bigint
  gives `bigint[]`; an empty constructor needs a type (`ARRAY[]::integer[]`).
  `pg_typeof(nextval('order_numbers'))` still issues one number. `OFFSET` is
  evaluated before `LIMIT`.
- Polymorphic built-ins choose their argument types before running, as
  PostgreSQL's parse analysis does. An untyped literal takes the type of the
  other arguments: `array_append(ARRAY['2020-01-01'::date], '2020-01-02')`,
  `greatest(1, '2')` and `int4range(1,5) @> '[2,3)'` work, and typed inputs
  of `anycompatible` functions convert to one common type
  (`array_replace(ARRAY[1], 1, 2.5)` is `numeric[]`). `timestamp + '1 day'`
  adds an interval; `date + '1'` fails with `42725` (`operator is not unique`).
- `DISTINCT`, `GROUP BY`, set operations and array search need a type's
  default equality: `SELECT DISTINCT '(1,2)'::point` fails with `42883`
  (`could not identify an equality operator for type point`), as for `xml`
  and `json`, even though `box = box` (area equality) works.
- `pg_sleep`, `pg_sleep_for` and `pg_sleep_until` do not hold the SQL worker:
  the statement waits on a timer (`pg_stat_activity` shows `Timeout` /
  `PgSleep`) while other connections run, and resumes without repeating work
  it already did, so `SELECT nextval('s'), pg_sleep(1), nextval('s')` returns
  consecutive numbers. `statement_timeout` and a cancel request end the wait
  with `57014`.
- Enum literals are checked before rows are chosen: `'missing'::order_status`
  raises `22P02` even in an unused `CASE` arm, while
  `'missing'::text::order_status` converts only when run.
- `aclitem[]` must be one-dimensional (`22023`, `ACL arrays must be
  one-dimensional`); `aclexplode(NULL)` returns no rows.
- Point coordinates use `(point(1,2))[0]`. Converting an open path to a polygon
  fails with `22023`.
- A PL/pgSQL row variable (`r t`, `r t%ROWTYPE`, `r composite_type`) has
  its declared row type from the start: while it is NULL, `pg_typeof(r.a)`
  names the field's declared type, `pg_typeof(r)` names the table, and an
  unknown field fails with `42703` (`record "r" has no field "zz"`). A field
  of an unassigned `record` variable fails with `55000`. `FOR r IN SELECT
  ...` fills a row variable from each row's columns.
- `x LIKE ANY (SELECT ...)`, `NOT LIKE ALL` and `ILIKE` match against a
  subquery's rows as they do against an array. The operator spellings `~~`,
  `!~~`, `~~*` and `!~~*` are `LIKE`, `NOT LIKE`, `ILIKE` and `NOT ILIKE`,
  quantified forms included (`x ~~ ANY (array['a%'])`). The POSIX operators
  `~`, `~*`, `!~` and `!~*` quantify the same way (`x ~ ANY (array['^a'])`),
  as does `OPERATOR(pg_catalog.~) ANY (...)`; a quantified user-defined
  operator fails with `0A000`. A pattern match between non-string operands
  (`1 LIKE 'a'`, or `1 ~ ANY (array['a'])` over an empty array too) fails
  with `42883` (`operator does not exist: integer ~~ unknown`).
- An unmatched PL/pgSQL `CASE` without `ELSE` raises `20000` (`case not found`,
  hint `CASE statement is missing ELSE part.`).
- PL/pgSQL bound cursors (`c [NO] [SCROLL] CURSOR [(lo int, hi int)] FOR
  query` in `DECLARE`) open with `OPEN c` or `OPEN c(2, hi := 3)`; arguments
  convert to their declared types and the query reads them by name, with
  the variables declared before the cursor (a cursor declared in a nested
  block's `DECLARE` section fails with `0A000`). As in
  PostgreSQL 18 the variable stays NULL until `OPEN` names the portal, and
  argument mistakes fail with `42601` when the routine is created.
  `FOR rec IN c LOOP` (or `FOR rec IN c(2) LOOP`) opens the cursor as a
  portal listed in `pg_cursors`, fetches one row per iteration into its own
  record `rec`, and closes the portal when the loop ends by any route. While
  the loop runs, `CLOSE c` fails with `24000` (`cannot drop pinned portal`)
  and an already open cursor fails with `42P03`; a variable that was NULL is
  NULL again after the loop. `COMMIT` or `ROLLBACK` inside a procedure's
  cursor loop keeps the loop reading, as PostgreSQL holds the portal.
- User error codes such as `PZ001` behave as ordinary errors: `WHEN SQLSTATE
  'PZ001'`, `'PZ000'` (category match), or `WHEN OTHERS` catch them.
- `pg_input_is_valid` and `pg_input_error_info` report invalid values, but an
  error raised inside a domain CHECK still fails, following PostgreSQL's
  [input-checking contract](https://www.postgresql.org/docs/18/functions-info.html#FUNCTIONS-INFO-VALIDITY)
  and [domain CHECK boundary](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/domains.c#L118):

  ```sql
  CREATE DOMAIN positive_count AS integer CHECK(VALUE>0);
  SELECT pg_input_is_valid('-1','positive_count'); -- false
  CREATE DOMAIN broken_check AS integer CHECK(VALUE/(VALUE-VALUE)>0);
  SELECT pg_input_is_valid('1','broken_check'); -- error 22012
  ```

#### Domains and checked inputs

- Arrays of domains over built-in arrays work (`integer_vector[]` for a domain
  over `integer[]`). Arrays of domains whose base is an array of user-defined
  values are rejected. A domain over an array of scalar domains
  (`CREATE DOMAIN positive_ids AS positive_integer[]`) works.
- Bind validates parameters before function inputs are prepared, so an invalid
  integer parameter fails at Bind. `PREPARE saved AS SELECT
  '{paid}'::checked_status[]` checks the literal once at `PREPARE`;
  `ARRAY['paid']::checked_status[]` checks when executed.
- `PREPARE` types every parameter as PostgreSQL does: declared types first,
  then the type its use selects. `PREPARE q AS SELECT round($1)` records
  `double precision` in `pg_prepared_statements`, so `EXECUTE q(2.5)` returns
  2. A parameter that is never used (`SELECT $2::int` leaves `$1` unused) or
  only passed where any type is accepted (`pg_typeof($1)`) fails with `42P18`;
  an otherwise untyped one is `text`. An untyped operand of a built-in
  operator takes the type PostgreSQL's operator selection picks
  (`jsonb -> $1` reads `text`); when it cannot pick one (`current_date + $1`)
  `PREPARE` fails with `operator is not unique` (`42725`). User-defined types
  are recorded exactly: `PREPARE q(positive)` and `PREPARE q AS SELECT
  $1::mood` list `positive` and `mood`, and `EXECUTE` checks a domain's
  constraints. `EXECUTE` converts each argument with PostgreSQL's assignment
  rules, so `EXECUTE q(2.7)` passes 3 to an integer domain and
  `EXECUTE q('happy'::text)` fails with `42804` (`parameter $1 of type text
  cannot be coerced to the expected type mood`); so does `EXECUTE q(5)` for
  an `orders` parameter. A protocol Parse fixes its
  parameter types and reports an undeterminable one with `42P18` at Parse;
  later Bind and Describe messages reuse those types.
- SQL functions are validated at `CREATE`, then each backend checks and keeps
  the first call's inputs for that definition and argument types. PL/pgSQL keeps
  one instance per resolved argument types and collation. Views, rules,
  `BEGIN ATOMIC` bodies, and stored defaults keep their checked inputs with the
  definition. See [plan lifetimes](architecture.md#the-request-path).
- In a SQL function body, `o.note` reads field `note` of a composite
  parameter `o` (also written `f.o.note` with the function's name `f`), unless
  a table or alias `o` in the query has a column `note`, which wins as in
  PostgreSQL. A field the parameter's type lacks fails with `42P01` (`missing
  FROM-clause entry for table "o"`).
- `DISCARD PLANS` drops cached routine plans for future calls; `DISCARD ALL`
  also drops prepared statements. Either returns `0A000` while such a plan is
  executing.

#### Dates and times

- Time zone abbreviations follow PostgreSQL 18.6's
  [Default set](https://github.com/postgres/postgres/blob/REL_18_6/src/timezone/tznames/Default)
  (checksum-pinned at build) and the session zone: `2021-06-01 12:00 EST` is
  17:00 UTC; with `TimeZone = 'Asia/Shanghai'`, `CST` means UTC+8.
  `timezone_abbreviations` cannot be changed. The named zones and their
  offsets are PostgreSQL 18.6's: its 598 zones, compiled from its tzdata 2026c
  and built into pos3ql (checksum-pinned at build), as PostgreSQL ships its own
  zone data. The host's zone files are not read, so
  `'2030-06-01 12:00 Europe/Paris'` has PostgreSQL 18.6's offset whatever
  tzdata release the host has.
- Numeric time zones are hours east of UTC (`SET timezone TO '2.5'` is
  UTC+02:30), within ±167:59:59; month/day interval fields return `22023`
  ([PostgreSQL rules](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/variable.c#L251),
  [bounds](https://github.com/postgres/postgres/blob/REL_18_6/src/timezone/localtime.c#L667)).
  `NaN` returns `0A000`.
- Date and time input decodes text as PostgreSQL 18.6 does
  ([`datetime.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/datetime.c)).
  Numbers read in the `DateStyle` field order: `'05/01/2024'::date` is May 1
  under `MDY` (the default) and January 5 under `DMY`; `YMD` reads `24/05/01`
  as 2024-05-01. A year of three or more digits, a month name (`Jan 5 2024`,
  `5-Jan-2024`), a Julian day (`J2451187`), run-together digits (`20240105`,
  `20240105T123456`), a day of the year (`2024.005`), `BC`/`AD`, `AM`/`PM`,
  zone names, abbreviations and offsets, and the words `epoch`, `infinity`,
  `now`, `today`, `tomorrow`, `yesterday` and `allballs` work in any order.
  Errors carry PostgreSQL's SQLSTATE and message: `'13/01/2024'::date` fails
  with `22008` and the hint `Perhaps you need a different "DateStyle"
  setting.`; an unknown zone name such as `Mars/Olympus` fails with `22023`.
  BC dates print with a ` BC` suffix (`0044-03-15 BC`). A name that is not an
  installed zone is read as a POSIX zone specification, as PostgreSQL reads
  it, in input, `AT TIME ZONE` and `SET TimeZone`: the offset counts hours
  *west* of UTC, so `UTC+3` is three hours behind UTC and `xyz5` five; a
  daylight-time name follows (`EST5EDT`, an hour ahead unless an offset is
  given) with rules such as `,M3.2.0,M11.1.0` (`Mm.w.d`, `Jn` or `n`, with an
  optional `/time`), the United States rules when none are given. `SHOW
  TimeZone` reports the specification upper-cased (`XYZ5`), whose letters are
  also the zone's abbreviations.

## Transactions, cursors and application behavior

### Transaction modes

Every configuration sets `transaction_mode`, `catalog_history_bytes` and
`row_history_slots` explicitly:

| Mode | Reader and writer behavior | Isolation | History settings |
|---|---|---|---|
| `mvcc` | Readers keep seeing earlier values while they need them | PostgreSQL snapshot and isolation behavior | Positive |
| `locking` | Readers see current committed values; conflicting relation operations wait | Read committed (and read uncommitted, which PostgreSQL treats the same); repeatable read, serializable and exported snapshots return `0A000` | Zero |

Rollback, savepoints and private uncommitted changes work in both modes.
Row-history slots are shared by all tables and large objects; an update
reserves its slot before commit and rollback releases it. A long report can
hold history until the fixed pools fill; the server then returns a named
capacity error instead of growing memory.

A commit becomes visible all at once, and only after it is durable. If commit
waits for storage, completed work (an issued order number, a trigger's audit
row) is not repeated. Commit can be cancelled until it is published. Prepared
transactions keep their locks and private catalog changes across a restart: a
domain created before `PREPARE TRANSACTION 'import'` stays hidden from other
connections until `COMMIT PREPARED 'import'`. A lost final reply leaves the
client unsure whether the commit happened; check application state before
retrying an operation that could charge twice. If the server cannot install a
commit after it became durable, it rejects further work with `08007` and
requires restart and recovery; it never undoes durable changes.

### Foreign keys

**Referential actions.** `ON DELETE`/`ON UPDATE` `CASCADE`, `SET NULL` and
`SET DEFAULT` run when the statement that deleted or re-keyed the referenced
rows ends, in the order of that row's `AFTER` events, and are never deferred,
even for a `DEFERRABLE` key. So `UPDATE tree SET k = k + 10` on a
self-referencing `ON UPDATE CASCADE` table first renumbers every row and then
repoints each child. A `SET DEFAULT` whose default is the deleted key fails
like `NO ACTION` (`23503`, `Key (...) is still referenced`). Each action runs
PostgreSQL's own query on the referencing table (`DELETE FROM ONLY
"public"."lines" WHERE $1 OPERATOR(pg_catalog.=) "order_id"`) as that table's
owner: its rules rewrite it, its `BEFORE` triggers fire, an error inside it
names that query in `CONTEXT`, and its `AFTER STATEMENT` trigger fires once
with every cascaded row in its transition table. A rule that leaves no
`DELETE` (`DO INSTEAD NOTHING`) fails the action with `XX000`. `ON DELETE SET
NULL (owner)` changes only the listed columns (`pg_constraint.confdelsetcols`).

**Different key types.** A referencing column may differ in type from its key
whenever PostgreSQL 18 accepts the pair (`integer` referencing `bigint`,
`varchar` referencing `text`, `timestamp` referencing `date`), comparing with
the key type's cross-type equality or after converting to the key's type, as
PostgreSQL does; `pg_constraint` reports the operators in `conpfeqop`,
`conppeqop` and `conffeqop`. `ON UPDATE CASCADE` of a `bigint` key too large
for an `integer` column fails with `22003`. Other pairs fail with `42804`
(`foreign key constraint "orders_customer_fkey" cannot be implemented`), also
when `ALTER COLUMN ... TYPE` would leave a key in that state. Composite keys
compare field by field (`cannot compare dissimilar column types integer and
bigint at record column 1`, `42804`). Keys that need a user-defined implicit
cast fail with `0A000`.

**Concurrent writers.** Inserting or updating a referencing row locks the
parent row `FOR KEY SHARE`. If one session deletes customer 7 while another
inserts an order for it, the insert waits; it fails with `23503` when the
delete commits and succeeds when it rolls back. Under `REPEATABLE READ` and
`SERIALIZABLE`, a parent deleted or re-keyed after the snapshot fails the
insert with `40001`; a partitioned parent is read with a current snapshot, as
PostgreSQL's check does. A check that waited keeps its starting snapshot, so
under read committed a parent deleted and reinserted with the same key while
it waited does not count (`23503`). Changing other parent columns, or
`UPDATE customers SET id = id`, does not conflict. Each locked row takes one
entry of the row-lock registry, (`max_connections` +
`max_prepared_transactions`) × `txn_rows` entries in total; a full registry
fails the statement with `54000`.

### Rows changed by another transaction

`UPDATE`, `DELETE`, `MERGE` and `SELECT ... FOR UPDATE` (and the other lock
strengths) lock each row they select. When another transaction changed that
row after the statement's snapshot, `READ COMMITTED` continues with its latest
committed version, as PostgreSQL does: `UPDATE accounts SET balance = balance
+ 10` that waited for another session's `+ 1` stores both, a row that no
longer meets the `WHERE` condition is skipped, and a deleted row is not
changed again. `REPEATABLE READ` and `SERIALIZABLE` fail with `40001` (`could
not serialize access due to concurrent update`, or `concurrent delete`);
`FOR KEY SHARE` still succeeds when every unique key column kept its value.

- A row another transaction moved to another partition cannot be followed:
  a statement that waited for it fails with `40001` (`tuple to be locked was
  already moved to another partition due to concurrent update` under `READ
  COMMITTED`); retry it. A waiting foreign key check fails the same way. The
  move is journaled, so a prepared transaction's move still fails waiting
  statements after a restart.
- `SELECT ... FOR UPDATE` returns the locked row's latest version: join
  conditions, `WHERE` and output run again over it while other tables keep
  the rows first read, and the result keeps its order. A job queue's `SELECT
  id FROM jobs WHERE state = 'new' ORDER BY id LIMIT 1 FOR UPDATE` that waited
  skips the job another worker claimed.
- `MERGE` chooses its action again for the target row's latest version, as in
  PostgreSQL 15 and later, running `WHEN NOT MATCHED` once per lost match.
- `WHERE` is rechecked as PostgreSQL's plan does: a top-level uncorrelated `IN
  (subquery)` or correlated `EXISTS (subquery)` is a semi-join, so the row stays
  only if the subquery row it first matched still matches. pos3ql takes the
  first match in its own scan order, which equals PostgreSQL's for a
  sequential scan of one table. Other conditions, and subqueries PostgreSQL
  keeps as subplans, run again over the latest version.
- `UPDATE ... FROM` and `DELETE ... USING` recheck a changed target row
  against the `FROM` row it joined, then the next row its first version
  joined; they never join the new version to other rows. An account moved
  from tier 1 to tier 2 while `UPDATE accounts SET rate = r.rate FROM rates r
  WHERE accounts.tier = r.tier` waited is skipped.
- A `LATERAL` source follows the same plan rules: a simple subquery without
  `FROM` runs again; a function, a several-row `VALUES` list or a subquery
  with aggregates, grouping, `DISTINCT`, `LIMIT` or `OFFSET` keeps the row
  first read; a simple subquery reading tables keeps its joined rows and
  reruns its `WHERE` and output. When several of its rows produced the same
  values but recheck differently, or it is a `UNION ALL` branch that reads
  tables, the recheck fails with `0A000`.

A partitioned or inherited table with foreign children reads and changes their
remote rows with its local ones; `MERGE` into it fails with `0A000` (`cannot
execute MERGE on relation "orders_archive"`). Without `ORDER BY`, reads and
`RETURNING` rows follow PostgreSQL's order: partitions in bound order (the
default partition last, a `NULL`-only list partition after the other list
partitions, hash partitions by modulus and remainder), inheritance children
level by level in creation order.

UPDATE, DELETE, MERGE, `ON CONFLICT` and validation keep the selected row
values; if the table or its definition changed before the write is installed,
the statement returns `40001`. A full rollback log rejects only the change it
could not record. A physical table rewrite follows PostgreSQL's
[MVCC caveat](https://www.postgresql.org/docs/18/mvcc-caveats.html): an older
repeatable-read transaction that first opens the table after the rewrite can
see it as empty.

### Temporary objects, sessions and accounts

Temporary tables, views and sequences belong to their connection and are
never written to object storage; named types and routines need ordinary
schemas. `pg_temp` qualifies table names. `DISCARD TEMP` drops temporary
objects with `DROP`'s dependency checks, and rollback restores them. Outside a
transaction `DISCARD ALL` also clears prepared statements, cursors, settings,
notifications, sequence session state and session advisory locks; `DISCARD
SEQUENCES` forgets remembered sequence values without rewinding them.

A connection follows the account that logged in through renames: after `ALTER
ROLE orders_app RENAME TO orders_writer; CREATE ROLE orders_app LOGIN`, the
existing connection is `orders_writer` and the new `orders_app` gets none of
its permissions or cancellation rights. If that account is dropped, constant
queries still work, `current_user` returns `42704` and object creation `42501`.
Recreating a dropped owner's name does not grant the right to resolve its
prepared transactions.

Memberships and grants follow PostgreSQL: grants by different grantors are
independent (repeating one emits a NOTICE, revoking a missing one a WARNING);
changing ADMIN, INHERIT or SET keeps the membership's identifier; owner
privileges follow inherited membership separately from `SET ROLE` (with
INHERIT FALSE and SET TRUE an `app` role must `SET ROLE owners` before altering
their table; with INHERIT TRUE and SET FALSE it can alter it, but `SET ROLE
owners` returns `42501`); revoking an owner's own SELECT keeps its right to
grant it; default privileges keep their identifier while any grant remains.
Permission changes on one object wait for that object's open transaction. A
privilege check on a missing object by OID returns NULL for a table, sequence
or large object, and true for a superuser (NULL otherwise) for the other
object kinds.

### When volatile expressions run

Sorting postpones output expressions until the result order is known:

| Query | `nextval` calls |
|---|---|
| `SELECT id, nextval('order_numbers') FROM orders ORDER BY id LIMIT 1` | 1 |
| Same with `OFFSET 1` | 2 (PostgreSQL also evaluates the skipped row) |
| Same with `FETCH FIRST 1 ROW WITH TIES` | 2 (the next row is checked for a tie) |
| `ORDER BY nextval(...)` | one per candidate row, reused for output |
| Sorted subquery with `LIMIT 0` | 0 |

Expressions evaluated before sorting can still fail on later input rows, and
spilling a sort does not change these effects. A paged portal evaluates rows
only as each `Execute` asks, so `CASE WHEN id > 1 THEN nextval('s') END` takes
nine numbers for a first page of ten rows.

- Skipped `CASE` and procedural branches do not run, but ordinary SQL can
  prepare some uncorrelated subqueries in a skipped branch. `GREATEST`,
  `COALESCE`, `BETWEEN` and JSON and XML constructors evaluate arguments as
  PostgreSQL does (`1 BETWEEN 2 AND NULL` is false; a sequence call inside
  `BETWEEN` can issue more than one value).
- Immutable constant expressions can fail during planning even when a false
  filter removes every row, following PostgreSQL's
  [volatility rules](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/include/catalog/pg_proc.dat);
  volatile calls such as `brin_summarize_new_values('orders_brin')` run only
  when reached. Constant catalog casts validate without rows:
  `'missing_table'::regclass` over an empty table raises `42P01`.
- A `STRICT` function with a constant null argument is skipped without
  evaluating its other inputs (also in defaults and an explicitly null
  variadic array); in `FROM` it evaluates its inputs first. Given
  `import_batch(bigint, bigint)`, `CALL import_batch(nextval('s'), 'bad')`
  fails before issuing a number, while `... nextval('s'), 1/0)` issues one.
  Set-returning functions evaluate their inputs once.
- Ordinary `nextval` is not rolled back; `ALTER SEQUENCE ... RESTART` is.
  Sequence values issued before a wait are reused when the command resumes;
  if recording them exceeds statement memory the command fails with `54000`
  (`volatile sequence replay exceeds statement memory`).

Conversions that differ from intuition but match PostgreSQL: `NULLIF` uses the
selected equality operator (`pg_typeof(NULLIF(1, 2.2))` is `numeric`); `real`
and `double precision` convert to `numeric` with six and fifteen significant
digits; registered cast functions receive the type modifier and explicit flag
(`SELECT 7::varchar(5)` passes `9` and true); array casts prefer a whole-array
cast, then convert elements in order keeping bounds; array comparison
functions match the operators (`ARRAY[NULL::integer] > ARRAY[1]` is true); SQL
JSON constructors use registered enum-to-JSON casts; a polygon assigns to a
path column, while a box needs `::point` for a point column. Sorting keeps whether a
joined row existed, so `row_to_json(profile)` tells a missing profile from an
all-null one.

### Cursors

SQL cursors save typed values and format each fetched row with the session's
current settings (a saved `DATE '2020-01-02'` displays as `02/01/2020` after
`SET DateStyle = 'SQL, DMY'`). Every cursor reads the snapshot taken when it
was declared or opened, so a cursor a `BEFORE` trigger opens reads the row its
statement inserts after the trigger returns; rows changed later stay
invisible. Until its query has run, a cursor holds a statement snapshot slot:
`max_connections` × `max_cursors` such slots exist beyond executing
statements, and a full table raises `54000` (`statement snapshots are
exhausted`). Opening a PL/pgSQL cursor captures its inputs; dynamic SQL sees
only its `USING` arguments.

**Reading as you fetch.** A cursor over one table, constant `VALUES` rows, a
builtin set-returning function with constant arguments (`generate_series`,
`generate_subscripts`, a one-array `unnest`, `regexp_matches`,
`regexp_split_to_table`, the `json`/`jsonb` `_each`, `_each_text`,
`_array_elements`, `_array_elements_text` and `_object_keys` functions, alone
or in `ROWS FROM`, with or without `WITH ORDINALITY`), or a join of these
(inner, cross or `LEFT`, or a `RIGHT` or `FULL` join of two items, with a
non-volatile condition) reads input only as rows are fetched when its query
uses columns, constants, parameters, built-in operators and casts, boolean
logic, `CASE`, `BETWEEN`, null tests, arrays, subscripts, scalar functions,
`LIMIT` and `OFFSET`. A one-table cursor, row-locking ones included, may also
order by its columns. A join hashes its build table into `plan_build_bytes`
at `DECLARE` (a table that does not fit makes the cursor save its rows then);
a join the engine reads with index lookups per outer row runs at `DECLARE`.
Other cursors (other orderings, user-defined operators, row security, other
function sources such as `jsonb_to_recordset(...) AS t(a int)`) run whole at
their first `FETCH` or `MOVE`, as PostgreSQL first runs a portal there.

- `DECLARE c NO SCROLL CURSOR FOR SELECT nextval('s')` issues no number
  until the first nonzero fetch; `generate_series(1, 70000)` keeps only its
  next value, and its argument errors come from the first `FETCH`. Type and
  constant errors (`1/0`) are reported at `DECLARE`.
- `SELECT 100/id FROM orders WHERE id<>0` divides only fetched rows. `LIMIT
  2 OFFSET 3` skips three matching rows, still evaluating their output list.
  Constant false or null filters read nothing.
- A forward `FETCH n` or `FETCH ALL` from the client streams rows as they are
  evaluated, saving none; `MOVE` skips without saving output.
- A row-locking cursor locks each row when a `FETCH` or `MOVE` reaches it,
  after evaluating its output and before `OFFSET` counts it. A fetch that
  meets another transaction's lock waits; under `READ COMMITTED` a row changed
  after `DECLARE` is filtered and projected again from its newest version,
  under `REPEATABLE READ` and `SERIALIZABLE` the fetch fails with `40001`.
  Rows the cursor's own transaction changed after `DECLARE` are skipped. A
  trigger can `FETCH` from such a session cursor.
- An ordered cursor sorts at its first `FETCH`, saving the columns it reads
  of every passing row in `cursor_bytes` (whole rows if it locks; `54000`
  when full), and evaluates each row's output when a fetch reaches it, so
  `SELECT v, nextval('s') FROM t ORDER BY id` takes numbers in sorted order.
- Under serializable isolation, a table counts as read once fetching reads it.

**Scrolling.** Without `SCROLL` or `NO SCROLL`, a cursor is scrollable when
PostgreSQL's plan for it can run backward and it locks no rows; pos3ql decides
from the query after views and inlined `WITH` queries expand. Table, function,
multirow `VALUES` and recursive `WITH` scans, an `ORDER BY` over a `FROM`
clause (unless its first key is a column an equality join merges on),
`LIMIT`, and a `UNION ALL` of scrolling branches scroll. Joins (other than with
a `FROM`-less one-row subquery), grouping, aggregates, `DISTINCT`, `DISTINCT
ON`, window functions without an `ORDER BY` above them, set-returning output,
`TABLESAMPLE`, `UNION`, `INTERSECT`, `EXCEPT`, a `FROM`-less query and a
top-level `IN` or correlated `EXISTS` condition do not. `pg_cursors.is_scrollable`
shows the choice; a backward fetch from a cursor that does not scroll raises
`55000`.

A scrollable cursor whose output and filter call no volatile function keeps
only its current fetch's rows; moving back further reads the input again
from its first row under the cursor's snapshot (`seq_tup_read` counts the
rereads), and `FETCH PRIOR` right after a fetch reuses its rows. With a
volatile output such as `nextval`, a revisited row is evaluated again, as
PostgreSQL scans backward (`FETCH NEXT` twice then `FETCH PRIOR` returns row 1
with the third number); the cursor saves each row's input columns, and a
volatile filter also saves the rows it rejected. A `SCROLL` join cursor with a
volatile output or filter returns a revisited row's first values, as
PostgreSQL's Material node does. `MOVE` and `FETCH ABSOLUTE` take PostgreSQL's
steps. History must fit `cursor_bytes` ([capacity](#cursor-capacity)).

**Holding.** `WITH HOLD` saves the unread rows at commit, also when declared
outside `BEGIN`; a scrollable cursor that re-evaluates rows is saved whole
and keeps its position. If saving fails (an unread row with `id=0` makes
`SELECT 100/id` fail at `COMMIT`), the commit rolls back and closes new
cursors; cursors held earlier survive. While a cursor is open, the same
session cannot `DROP`, `TRUNCATE` or structurally `ALTER` its table; renaming
is allowed.

### COPY and generated columns

Table-form `COPY TO` sends `SELECT columns FROM ONLY table` in a plain
`SELECT`'s order; it returns `42809` for a partitioned parent without a
row-security rewrite and `0A000` for an unpopulated materialized view, and
views, foreign and catalog tables need the query form (`COPY (SELECT id,
status FROM orders ORDER BY id) TO STDOUT (FORMAT csv)`). A large `COPY ... TO
STDOUT` from one table or a streamable query resumes row by row like a large
`SELECT`.

`COPY FROM` statement triggers with `REFERENCING NEW TABLE AS inserted_rows`
see the values saved after `BEFORE` triggers and generated columns. These rows
use `copy_transition_bytes` plus `txn_rows` records per connection and per
subscription worker; exhaustion returns `54000` and aborts the COPY. Deferred
constraints run before every commit, including after COPY and at an extended
protocol Sync, and a failed check rolls back the command. COPY keeps its user,
`search_path`, formatting settings, timestamps and deadline across data
messages.

A generated column is `VIRTUAL` unless `STORED` is written, as in PostgreSQL
18 (`attgenerated = 'v'`): queries read its computed value, a trigger's `OLD`
and `NEW` hold NULL for it, and COPY reads and writes no generated column.
Indexes, keys, foreign keys, extended statistics, domain or user-defined
types, user-defined functions in its expression, `DROP EXPRESSION`, and `SET
EXPRESSION` on a table with CHECK constraints are refused with PostgreSQL's
errors.

### Procedural code and deadlines

- A trigger can end in a nested `RETURN` or `RAISE`; reaching the end without
  returning fails with `2F005`. Procedural expressions can run subqueries over
  variables and trigger rows; a scalar subquery with no rows is NULL and with
  more than one fails with `21000`.
- A numeric `FOR` loop declares its own integer variable; query `FOR` loops
  and `FOREACH` assign the declared target, which keeps the last value; an
  empty query loop assigns a null row and an empty array loop leaves the
  target unchanged
  ([reference results](https://github.com/e6qu/pos3ql/blob/bcbaba7ba47b735c123198a03d14e80935340a3e/tests/external/reference/2026-11-03-input-plans/README.md)).
- `EXCEPTION WHEN OTHERS` does not catch cancellation or assertion failures;
  name `query_canceled` or `assert_failure`. A caught block is a
  subtransaction: its row changes, schema edits, settings and queued deferred
  triggers are undone, local variables and issued sequence values kept,
  cursors opened inside it closed
  ([reference results](https://github.com/e6qu/pos3ql/tree/854feb372391d1c87f3d5b261e85a78430d96449/tests/external/reference/2026-11-05-procedural-subtransactions)).
- A command that waits and resumes does not repeat completed effects: a
  function's finished audit `INSERT` runs once however often its next
  `SELECT` waits for a lock. `CALL import_batches()` that commits each batch
  keeps the committed batches when a later batch waits, and a lock timeout or
  cancel then ends only the current transaction.
- DDL succeeds only after its end-of-command event triggers; a failing hook
  for `DO 'BEGIN CREATE TABLE orders(id integer); END'` leaves no table.

A statement keeps one deadline through waits and nested SQL. Extended-protocol
Parse, Bind, Describe and Execute share the timer until Execute or Sync
finishes, and an expired timer is reported on the next message. Procedural
code checks the deadline at every block and instruction; during COPY FROM a
row arriving after the deadline fails with `57014`
([PostgreSQL probe](https://github.com/e6qu/pos3ql/tree/02d030e49e5a483d567ffb205dd01c4b9faad857/tests/external/reference/2026-11-05-procedural-deadlines)).

### Object identity and bound references

Renames keep an object's OID; drop and recreate assign a new one, so a saved
OID never grants access to a replacement (`has_table_privilege(old_oid,
'SELECT')` is NULL). This covers relations, types, enum members, indexes,
constraints, statistics, extensions, foreign-data objects, publications,
subscriptions, collations, conversions, text-search objects, casts, operators
and their classes and families, access methods, event triggers, memberships,
default privileges, triggers, rules, routines and policies.

- Dropped columns keep their numbers and count toward the 1,600-column limit;
  removing a column or rule does not renumber the others. `ADD CONSTRAINT ...
  USING INDEX` keeps the index's OID; NOT NULL constraint names survive
  renames. Template copies keep the template's identifiers but edit
  independently.
- Inherited triggers keep their OID when disabled or renamed; each child keeps
  its own enabled mode, and a parent's statement trigger is not inherited.
- A paused comparison keeps its collation even if it is dropped; renaming
  enum label `pending` to `waiting` changes display without rewriting rows; a
  moved table keeps bound references, while string SQL bodies and text lookups
  can fail after a rename; `pg_type.typdefault` may keep a domain default's
  original text (`pg_get_expr(typdefaultbin, 0)` shows the current one).
- A relation reserves its row-type name. `search_path` skips schemas without
  USAGE.
- Validation, unique index builds included, checks all committed rows and the
  caller's own changes. `NOT VALID` skips existing rows where supported;
  validating a domain used inside stored arrays or composites returns `0A000`.

### Views, rules and catalog snapshots

`CREATE OR REPLACE VIEW` keeps the view's OID, row type, `_RETURN` rule,
owner, permissions and comments. A materialized view created `WITH NO DATA`
fails reads with `55000`; `REFRESH` needs MAINTAIN, runs as the owner and holds
an exclusive lock until the transaction ends, and rollback restores rows and
populated status. Its `_RETURN` rule's `pg_depend` rows block `ALTER COLUMN
... TYPE` of columns it reads (`0A000`); rule commands on it fail as in
PostgreSQL (`0A000`, `2BP01`).

Rewrite rules take the target's exclusive lock. A rule condition may use
subqueries over other tables and `OLD`/`NEW`. `CREATE OR REPLACE RULE
"_RETURN"` replaces a view's query when it returns the view's column names and
types. `pg_get_ruledef` and `pg_rules` deparse a rule as PostgreSQL does when
its condition is an expression over `OLD`, `NEW` and constants and each action
is `NOTIFY`, a query, `INSERT ... VALUES`/`SELECT`, `UPDATE` or `DELETE` with
optional `RETURNING`; other rules return their saved SQL, and their pretty
form fails with `0A000`.

Catalog listings follow the query's snapshot, while permission checks and role
names use current state, as in PostgreSQL: after another connection renames
`orders` to `sales`, an older repeatable-read report still lists `orders`
while `to_regclass('sales')` resolves the new name, and an older
`pg_get_viewdef` can show a prior query (failing with `XX000` if the table was
dropped), as PostgreSQL's
[rule lookup](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/ruleutils.c)
does. Information-schema views follow the pinned 18.6
[definitions](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql),
including hiding definitions from non-owners. `tableoid` must be named
explicitly; index vectors start at subscript zero (casting arrays back returns
`42846`); `pg_node_tree` values can be copied and deparsed, but client input
returns `0A000`; some catalog relations resolve by name but return `0A000`
when queried.

### Statistics and maintenance

`ANALYZE` reads committed rows plus the caller's changes and computes each
column's `pg_stats` row as PostgreSQL's `compute_scalar_stats` does, from a
sample of 300 rows per unit of the statistics target (30,000 by default): the
whole table in physical order when it is no larger, so a 2,000-row table shows
PostgreSQL's `null_frac`, `avg_width`, `n_distinct`, most-common values,
histogram and `correlation`; a larger table's sample is random, as
PostgreSQL's is. A target of 0 leaves a column's statistics as they are, and
an empty table keeps its earlier statistics. `avg_width` counts values as
stored (a compressed value at its compressed size, an out-of-line one at its
18-byte TOAST pointer). Ranges, multiranges, `tsvector` and types without a
default ordering keep only `null_frac`, `avg_width` and an approximate
`n_distinct`; `most_common_elems` is not gathered. As in PostgreSQL, a rollback
can leave `reltuples` changed, and `relhasindex` can stay true after `DROP
INDEX` until `VACUUM`.

Background maintenance runs between client messages and while idle, yields to
queries while it waits for storage, and never removes data active readers,
prepared transactions or backups need. At 85% heap use eligible writes can wait
(`Extension`/`CacheCapacity`) for space; reads and commits keep running.
Journal positions and internal counters never wrap: running out of journal
positions returns `54000` (`WAL LSN space exhausted`) and catalog reservation
errors name the exhausted OID range; the [durable format guide](durable-format.md)
owns the encodings.

### Statements that cannot yet pause

A statement that must wait for object storage where pos3ql cannot yet keep its
unfinished work fails rather than repeat effects, and rolls back its SQL
writes (issued sequence numbers stay consumed, as in PostgreSQL). The `0A000`
messages are:

| Message | Example cause |
|---|---|
| `expression wait requires an owned invocation log` | A routine call that waits inside an expression no saved statement owns, such as a SQL-language function body replayed by its caller in retained `AFTER` trigger work |
| `SQL row expression requires retained write progress` | A write without row progress (a data-modifying `WITH` inside a routine body) that changed one row, then calls a function for the next row that must wait |
| `expression restart requires owned volatile values` | Restarting would repeat an unsaved `random()` or clock value |

Constraint triggers deferred to `COMMIT` also return `0A000` for a reached
literal of an array of a named domain, such as
`PERFORM cardinality('{1}'::positive_integer[])`. The
[execution model](architecture.md#execution-model) lists which paths pause;
[PLAN.md](../PLAN.md#pause-and-resume-every-query-path) tracks the rest.

## Full-text search languages

Full-text search turns a document into lexemes, the normalized word forms a
`tsvector` holds. A language's stemmer reduces related forms to one lexeme,
so `to_tsvector('german', 'Häuser')` is `'haus':1` and matches a query for
`haus`. Every database has the text search objects PostgreSQL 18.6's initdb
creates: the `default` parser; the `simple`, `synonym`, `ispell`,
`thesaurus` and `snowball` templates; the `simple` dictionary and
configuration; and for each of 29 languages (`arabic` through `yiddish`, as
`\dF` lists them) a Snowball dictionary `<language>_stem` and a configuration
`<language>`. Their OIDs, `dictinitoption` text, token mappings (Hindi and
Russian stem all-ASCII words as English) and descriptions match the
PostgreSQL 18.6 reference image, so psql's `\dF`, `\dF+`, `\dFd` and `\dFt`
print the same tables. Each database keeps its own copy of these 66 objects,
so `max_text_search_objects` must hold 66 per database.

- `CREATE TEXT SEARCH DICTIONARY ... (TEMPLATE = snowball, Language = ...)`
  accepts the 29 languages and `porter`, the original Porter stemmer
  PostgreSQL also ships. `StopWords` names one of the 15 shipped stopword
  lists (`danish`, `dutch`, `english`, `finnish`, `french`, `german`,
  `hungarian`, `italian`, `nepali`, `norwegian`, `portuguese`, `russian`,
  `spanish`, `swedish`, `turkish`); the `simple` template accepts `StopWords`
  and `Accept`. There are no other stopword or dictionary files, so another
  name fails with `F0000` (`could not open stop-word file`), and a dictionary
  from the `synonym`, `ispell` or `thesaurus` template fails with `0A000`.
- Words are lower-cased as under PostgreSQL's C ctype, the only database
  ctype pos3ql provides: ASCII letters only, so
  `ts_lexize('german_stem', 'HÄUSER')` is `{hÄus}` in both.
- `SET default_text_search_config` resolves the configuration and stores its
  schema-qualified name (`german` becomes `pg_catalog.german`); an unknown
  configuration fails with `22023`. `text @@ tsquery` uses this setting.
- Limits: as in PostgreSQL, a word longer than 1,000 bytes skips stemming and
  stopwords. A word is stemmed in a fixed 4,096-byte buffer; a stem that
  would outgrow it fails with `54000`, which no word within PostgreSQL's
  limits reaches.

### Snowball provenance

The stemmers are a statement-for-statement Rust translation of PostgreSQL
18.6's generated C files, `src/sql/snowball/<language>.rs`, produced by
`tools/generate-snowball-stemmers.py` from
[`src/backend/snowball/libstemmer`](https://github.com/postgres/postgres/tree/REL_18_6/src/backend/snowball/libstemmer)
(each file's SHA-256 is in its header). PostgreSQL synced those files with
[Snowball](https://snowballstem.org) commit
[`d19326ac`](https://github.com/snowballstem/snowball/commit/d19326ac6c1b9a417fc872f7c2f845265a5e9ece)
(2025-02-19; the files say Snowball 2.2.0 generated them).
`src/sql/snowball/mod.rs` ports Snowball's runtime (`utilities.c`, `api.c`)
to a fixed buffer and carries Snowball's BSD 3-clause license. The stopword
lists in `src/sql/snowball/stopwords.rs` come from
[`src/backend/snowball/stopwords`](https://github.com/postgres/postgres/tree/REL_18_6/src/backend/snowball/stopwords),
which PostgreSQL took from snowballstem.org under the same license. The
inputs were read from the `REL_18_6` source archive
(`https://codeload.github.com/postgres/postgres/tar.gz/refs/tags/REL_18_6`,
SHA-256 `ff9b1187e3ab120282b9f3e09a5efba2425838b3a843150c5cb2f019f47ac400`,
retrieved 2026-11-13). Regenerate with:

```sh
python3 -I tools/generate-snowball-stemmers.py \
  --source /path/to/postgres-REL_18_6/src/backend/snowball \
  --output src/sql/snowball
```

### Default text search parser provenance

The `default` parser (`ts_parse`, `ts_debug`, `to_tsvector`, `ts_headline`)
is a port of PostgreSQL 18.6's
[`src/backend/tsearch/wparser_def.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/tsearch/wparser_def.c).
`tools/generate-text-parser-tables.py` reads that file and
[`src/include/common/unicode_nonspacing_table.h`](https://github.com/postgres/postgres/blob/REL_18_6/src/include/common/unicode_nonspacing_table.h)
from the same `REL_18_6` archive and writes the token types, per-state action
lists and character tables to `src/sql/text_parser/tables.rs` (each input's
SHA-256 is in its header). `src/sql/text_parser/mod.rs` ports the interpreter
(`TParserGet`) and its character tests for a UTF-8 database with the C ctype,
where every non-ASCII character is a letter. `src/sql/text_parser/headline.rs`
ports `ts_headline`: `hlparsetext` and `generateHeadline` from `ts_parse.c`,
`prsd_headline` and its cover selection from `wparser_def.c`, the
position-reporting query executor from `tsvector_op.c` and the option list
parser from `tsearchcmds.c`. Regenerate the tables with:

```sh
python3 -I tools/generate-text-parser-tables.py \
  --source /path/to/postgres-REL_18_6 \
  --output src/sql/text_parser/tables.rs
```

Differential fixture
`tests/external/differential_exact/391_text_search_parser.sql` compares
`ts_parse`, `ts_debug`, `to_tsvector` and `ts_headline` on URLs, e-mail
addresses, hosts, file paths, numbers, versions, XML tags and entities,
hyphenated words, mixed scripts and long tokens with PostgreSQL 18.6.

Differential fixture
`tests/external/differential_exact/390_snowball_text_search.sql` compares
sample words of every language, configurations, dictionary DDL and psql's
describe output with PostgreSQL 18.6.

## Stored value forms (TOAST)

PostgreSQL stores a wide value compressed (pglz) or out of line (TOAST, "The
Oversized-Attribute Storage Technique"), and passes that stored datum on
unchanged; functions such as `pg_column_size` report its stored size. pos3ql
models the same forms:

- A stored row over 2032 bytes keeps its widest values compressed or out of
  line as `heap_toast_insert_or_update` stores them; `pg_column_size`,
  `relpages` and `ANALYZE` read the stored forms (`966`). A row too wide for a
  heap page after toasting fails with `row is too big` (`986`). A `tsquery`
  has plain storage and stays as formed.
- A stored value keeps its form through column references, binary-coercible
  casts, `COALESCE`, `NULLIF`, `GREATEST`, `LEAST`, `CASE`, derived tables,
  sorts, joins, partition scans, `INSERT ... SELECT`, `CREATE TABLE AS`,
  `REFRESH MATERIALIZED VIEW`, `MERGE` and `UPDATE` (`967`-`969`). pglz reads
  a `tsvector`, `path` or `polygon` in its stored binary form. A PL/pgSQL
  special variable such as `TG_TABLE_NAME` and a scalar subquery's value are
  formed anew, and a datum of unknown form that neither pglz nor lz4 could
  compress is stored as one formed anew (`981`, `239`).
- A whole-row reference, `ROW(...)`, a field of either, and a stored
  composite with its fields (nested composites and array elements included)
  keep their stored forms (`987`, `990`). A dropped column of a table row type
  stored before `ALTER TABLE ... DROP COLUMN` is sized by the layout the table
  keeps when it was fixed-width by-value.
- A PL/pgSQL variable keeps the datum it is given (`datumTransfer`): a simple
  expression's result keeps the header the expression gave it (`y := 'lit'`
  reads 7 bytes, `z := v || ''` 9), a copy of a variable keeps that variable's
  form, and a query's result (`SELECT INTO`, a `FOR` or `FETCH` row, a
  procedure's outputs) is read from a result tuple, which packs a short value
  (`SELECT 'lit' INTO a` reads 4) and keeps a stored value's form (`SELECT big
  INTO w` reads 44). Records keep their fields' forms, and `FOR` loops over
  static or `EXECUTE` queries keep each row's forms (`992`).
- A routine's parameters hold the datums their caller passes (`psize(s)`
  reads 6, `psize('lit')` 7, `psize(big)` 44), also across retained inputs,
  saved invocation logs and suspended calls; a binary-coercible conversion
  keeps the form and a cast function forms a new datum (`psize(c)` of a
  `char(3)` reads 6). A value a query forms anew, such as `SELECT big || ''
  INTO w`, reads 3004 through `FOR`, `EXECUTE ... INTO` and `FETCH`, and a
  cursor's rows keep the forms its query gave them (`992`).

Forms pos3ql does not carry yet fail with `0A000` wherever the form decides a
size; [PLAN.md](../PLAN.md#close-application-compatibility-gaps) lists them.

## Physical access paths

| Method | Supported paths |
|---|---|
| Btree | Equality, composite leading-prefix/range probes, parameterized joins and DML, ordered and covering scans |
| Hash | Single-key equality, including expression/partial indexes, prepared statements, joins, and DML |
| BRIN | Bitmap equality/range and built-in range/network inclusion; explicit summarization |
| GiST | Built-in network, range/multirange, geometry, and full-text predicates; geometric distance ordering |
| GIN | Array, full-text, JSONB, and jsonpath bitmap predicates |
| SP-GiST | Built-in network, range, geometry, text ordering/prefix, and geometric distance ordering |

A query reading one table, a join of analyzed tables, and the scan of an
`UPDATE`'s or `DELETE`'s target choose their scans and joins by PostgreSQL 18's
costs and return rows in that plan's order; `EXPLAIN` prints the same plan.
[Query planning](query-planning.md) lists the covered shapes, the figures
`EXPLAIN ANALYZE` and `BUFFERS` report, and the planner settings. Where
PostgreSQL could choose a path pos3ql does not model, `EXPLAIN` fails with
`0A000` naming the shape, for example `EXPLAIN of a scan of one table with a
GiST index is not supported`.

Index names describe SQL behavior, not PostgreSQL's page layout; native index
callbacks are not available. MVCC visibility and SQL rechecks decide every
result. [Immutable index navigation](index-navigation.md) specifies pruning,
covering, and nearest-neighbor eligibility.

An exclusion constraint (`EXCLUDE USING gist (during WITH &&)`) may use the
B-tree (the default), hash, GiST or SP-GiST method with any commutative
operator of an operator class of that method that accepts the element's type,
as PostgreSQL checks it: `EXCLUDE (code WITH =)` on `text`, `EXCLUDE USING gist
(area WITH &&)` on `box`. An element may be a column, a function call or a
parenthesized expression (`EXCLUDE USING gist (int4range(a, b) WITH &&)`,
`differential_exact/356`), with a `COLLATE`, a built-in or user-defined btree
operator class, and under btree `DESC` and `NULLS FIRST/LAST`; the elements'
expressions together hold at most 256 bytes of stored text (`54000` beyond).
Operator class options on an element fail with `0A000`. A build sorts rows by
the equality elements and sweeps one range-overlap element in O(n log n)
comparisons, naming the first conflicting row in heap order (`746`, `792`,
`797`); for a GiST constraint without such elements PostgreSQL names the
partner its index scan finds first. `CLUSTER` and `REPLICA IDENTITY
USING INDEX` accept a primary key or unique constraint's index as well as an
ordinary index.

`REINDEX (TABLESPACE ...)` without `CONCURRENTLY` returns `0A000` for
constraint-generated indexes such as `orders_pkey`, or for a table, schema, or
database target that contains one, before changing anything. `CONCURRENTLY` can
move their replacements. Selecting a table's large-value index directly with
`TABLESPACE` also returns `0A000`.

### Partition column order

An attached partition may store columns in a different order. SQL expressions,
defaults, and `RETURNING` follow the table named in the command; stored rows,
row triggers, generated columns, and constraints follow the destination
partition. This inserts `(1, 30)`:

```sql
CREATE TABLE orders (id integer, amount integer) PARTITION BY RANGE (id);
CREATE TABLE early_orders (amount integer, id integer);
ALTER TABLE orders ATTACH PARTITION early_orders FOR VALUES FROM (0) TO (10);
INSERT INTO orders VALUES (1, 30) RETURNING id, amount;
```

A generated column cannot be a partition key (`42P17`), and a key that is an
expression (`PARTITION BY RANGE (lower(name))`) fails with `0A000`. `ON CONFLICT DO UPDATE`
cannot move a row to another partition (`0A000`).

## Capacity boundaries

Limits come in three kinds: PostgreSQL's own limits, smaller fixed pos3ql
limits, and startup reservations you size in the configuration. Memory is fixed
at startup, so running out returns a named error, usually SQLSTATE `54000`
(program limit exceeded), instead of growing. A supported SQL shape can still
exceed the statement memory you reserved.

**User example:** adding a 65th constraint fails with `54000` even with memory
free. Filling `max_tables` also fails, but you can raise it before the next
start. Raising a reservation never lifts a fixed limit.

The [architecture size table](architecture.md#memory-and-physical-size) covers
block, row, heap, and instance-wide memory; the
[performance guide](performance.md#estimating-bandwidth-and-throughput) explains
why none of these numbers is a throughput rate.

### Fixed SQL limits

| Limit | Value | Error |
|---|---|---|
| Columns per table (including dropped), view, composite, or record | 1,600 | PostgreSQL's |
| Result columns (SELECT list, routine results, Describe/Bind formats) | 1,664 | PostgreSQL's |
| Routine input arguments | 100 | PostgreSQL's |
| Index or partition key attributes | 32 | PostgreSQL's |
| Grouping sets / CUBE elements / GROUPING arguments | 4,096 / 12 / 31 | PostgreSQL's |
| Wire and SQL `PREPARE`/`EXECUTE` parameters | 65,535 | PostgreSQL's |
| Array elements | 65,535 | named error |
| Constraints per table kind; CHECKs per domain | 64; 64 | `54000` |
| LIST partition bounds, inheritance parents, trigger arguments, routine configuration entries | 64 each | named error |
| Constraint and other catalog creation numbers (statistics, extensions, FDW objects, publications, subscriptions) | 1–9,999,999 | `54000` |
| Stored default expression, including saved display text | 120 bytes | `54000` |
| Identifier length | 63 bytes | Truncated at a character boundary with PostgreSQL's `NOTICE` (`42622`) |
| `search_path` | 128 bytes | `22023` |
| `application_name` after PostgreSQL's [ASCII cleaning](https://github.com/postgres/postgres/blob/REL_18_6/src/common/string.c#L81) (`\xhh` escapes) | 64 bytes | `22023`, prior value kept |
| Connection startup option word | 256 bytes | `54000` |
| Custom parameters (`SET myapp.tenant = '7'`) a session defines; name; value | 8; 63 bytes; 128 bytes | `54000` |
| Timeout settings | 2,147,483,647 ms | `22023`; enabling a disabled-only timeout is `0A000` |
| Simultaneously tracked function calls (`track_functions`) per SQL worker | 1,024 | `54000` |
| Error message text, including a `RAISE` message | 27,824 bytes | `54000` |
| `RAISE ... USING DETAIL` or `HINT` text | 12,288 bytes each | `54000` |
| Built-in function NOTICE/WARNING messages raised before the next output message; one message's text | 64; 256 bytes | `54000` |
| Transaction identities and command counters | 32-bit, checked | `54000` before reuse |
| `json` / `jsonb` nesting; jsonpath depth | 14,521 / 14,520; 128 | `54001`; see [SQL/JSON](sql-json.md) |
| `tsquery` input parenthesis nesting; output depth | 7,685; 16,340 (CI's pinned `postgres:18` image) | `54001` |
| Btree index entry | 2,704 bytes | `54000` (`index row size ... exceeds btree version 4 maximum 2704`) |
| XML nesting, XPath work, constructor output | see [SQL/XML](sql-xml.md) | program-limit error |

### Startup reservations

Each of these is set in the configuration file (see
[operations](operations.md)) and validated before startup. Slot and lease
counts must be 1..4,294,967,295; byte pools must be positive multiples of 64.
Exhaustion returns `54000` naming the pool, before any partial effect.

| Setting(s) | Bounds |
|---|---|
| `sql_arena_bytes` (per-connection statement memory), `work_arena_bytes` (per query workspace; see [`work_mem`](#work_mem)) | Row images, joins, lists, programs, JSON/XML work, table-function rows, sorts and aggregates, retained scan/COPY/validation records (32 bytes per resident row) |
| `catalog_image_bytes` (per query workspace, default 8 MiB) | Catalog relation images a query that calls a user routine builds when it starts, so its own catalog scans read the catalog as it began |
| `max_tables`, `max_extended_statistics`, other catalog `max_*` | One capacity per catalog family; omitting `max_extended_statistics` derives it from `max_tables`, so set it explicitly |
| `max_connections`, `max_prepared_transactions`, `max_subscriptions` | Transaction identity registry has `max_connections + max_prepared_transactions` entries |
| `max_catalog_versions_per_object`, `max_ddl_per_transaction` | Retained definition/undo versions. Enum label images use `max_enums + P` slots, `P = min(max_enums * max_catalog_versions_per_object, (max_connections + max_prepared_transactions + max_subscriptions) * max_ddl_per_transaction)`, each holding `max_enum_labels_per_type` labels |
| `row_history_slots`, `max_row_versions_per_row` | Shared MVCC row history; per-row pending/committed versions. Locking mode requires zero history slots |
| `row_overlay_slots`, `large_object_pages` | Resident row metadata cells |
| `row_work_bytes`, `row_work_leases` | Retained row-ID lists, DML buffers, and loop inputs; fragmentation can block a request despite free bytes |
| `table_metadata_bytes`, `table_metadata_leases` | Schema images and routine input values (starter 64 MiB, 8,192) |
| `statistics_bytes`, `statistics_leases` | Column statistics (starter 1 MiB, 1,024) |
| `index_statistics_slots` | Index counters (starter 4,096) |
| `deferred_trigger_bytes`, `trigger_event_spill_bytes` (required; `0` disables spilling) | Queued `AFTER` trigger and foreign key events: per-connection memory pages, then a local file shared by all connections |
| `error_text_bytes`, `error_text_leases` | Error messages, DETAILs and HINTs longer than 192 bytes, kept whole; text that does not fit fails with `54000` |
| `definition_image_slots` | Schema pins held by executing statements |
| `execution_control_bytes`, `execution_control_leases` | PL/pgSQL source images, locals, loop frames, saved outputs, caught errors, and the step records of a running multi-step write |
| `write_step_levels` (optional, default 4, at most 64) | Levels of multi-step statements (rule actions on tables with rules, rules on `WITH` targets) nested in one another's steps whose progress a write keeps; a routine call after rows changed one level deeper fails with `54000` naming the setting |
| `routine_input_slots` | Per-backend routine plan instances |
| `max_join_relations` (optional, default 128, 2 to 255) | Relations one query's join search plans; a query of more fails with `54000` naming the setting. Reserves the genetic join search's pool, about 2 KB per relation, shared by all connections; see [query planning](query-planning.md#joins) |
| `max_prepared`, `prepared_bytes` | Prepared statements per namespace and bytes per statement, shared with bound portals |
| `cursor_bytes`, `cursor_query_bytes` | See [cursors](#cursor-capacity) |
| `conn_send_buffer_bytes`, `result_spool_bytes`, `result_spool_leases` | Result output: the send buffer, then rows queued in the shared spool for a slow client (a private reserve of the send-buffer size holds closing messages). Only a client lagging by more than the spool gets `54000`; the largest single message must fit the send buffer. A paged portal (`Execute` with a row limit) that cannot stream (another shape, or suspended portals hold every `result_stream_slots` stream) buffers its result in `portal_result_bytes` and fails with `54000` naming it when full |
| `plan_build_bytes`, `plan_build_leases` (required; starter 16 MiB and 64) | Shared build sides of streamed sorts and streamed or cursor-read hash joins; a build that does not fit runs the statement as it ran without streaming (the result spools; a cursor saves its rows at `DECLARE`) |
| `result_stream_bytes` (required; starter 256 KiB, minimum 64 KiB) | Read position of each large `SELECT` from one table, builtin set-returning functions or a join of them that resumes row by row, including a paged portal between its `Execute` messages; a larger estimate, or a stream that outgrows it while opening, spools the result instead; a stream that outgrows it after its first row fails with `54000` naming the setting |
| `result_stream_slots` (required; starter 2, at least 1) | Result streams per connection: one running statement plus suspended paged portals. A paged portal opened while every slot is held buffers in `portal_result_bytes` |
| `txn_rows`, `copy_transition_bytes` (256 KiB default) | Streamed COPY statement-trigger inputs per connection |
| `commit_queue_slots` | Commits waiting for durability |
| `memtable_bytes` (≤ `u32::MAX`) | Two heap buffers are reserved; reclamation returns `54000` while both are retained |
| `object_store_get_slots`, `object_store_put_slots`, `object_store_block_put_slots`, `wal_upload_buffer_bytes` | Concurrent object-store requests; see [object storage](object-storage.md) |
| `statement_restart_waits` (optional, default 64) | Blocks one statement's restarts may download, per read owner, before a restart without progress reads the rest synchronously; see [execution model](architecture.md#execution-model) |
| `checkpoint_manifest_bytes` and backup/merge/cleanup rosters | Maintenance work per step |

Fixed per-query-workspace limits: 1,024 snapshot and file-owner slots, 64 scan
sources, 64 point-read sources, and 64 copied point reads. At 85% heap use,
writing commands wait for cache reclamation before taking effect; reads,
commits, and cancellation proceed.

### `work_mem`

PostgreSQL's `work_mem` is a per-session hint: how much memory a sort or hash
may use before it spills to temporary files. It never changes query results.
pos3ql accepts and reports it exactly like PostgreSQL 18, with the same units
(`B`, `kB`, `MB`, `GB`, `TB`), rounding, range (64 kB to 2,147,483,647 kB),
default (`4MB`), errors, `SHOW`, `pg_settings` row and routine `SET` clause.
For example, `CREATE PROCEDURE nightly() ... SET work_mem = '256MB'` works,
and `SHOW work_mem` inside it reports `256MB`.

In pos3ql the value decides when a sort, hash join, hash aggregate, window or
`WITH` query store spills, where PostgreSQL's would, so row order and
`EXPLAIN ANALYZE` figures match ([query planning](query-planning.md#sorting-hashing-and-memory-figures)).
It never sizes memory: spilled sorts and stores go to external runs on the
block grid when an object store is attached, a hash aggregate's spilled rows
to 8 kB blocks of `plan_build_bytes`, and everything else stays in the query
workspace of `work_arena_bytes`, fixed at startup. Exhausting any of these
fails with `54000` naming it, whatever `work_mem` says; raise the reservation
and restart.

After a session runs or explains a hash aggregate (a hashed `GROUP BY` or
`DISTINCT`) with `work_mem` below 1MB, PostgreSQL may reuse that
aggregate's memory for a later one, changing its row order in ways pos3ql
cannot follow. From the next statement on, such a later hash aggregate
fails with `0A000` ("the memory of a hash aggregate after an earlier hash
aggregate of this session ran with work_mem below 1MB is not supported")
while its table is small. For example:

```sql
SET work_mem = '64kB';
EXPLAIN SELECT DISTINCT a FROM big;   -- plans a 64kB hash aggregate
SELECT DISTINCT a FROM big;           -- 0A000 once its table outgrows 1 kB
```

Start a new session, or keep `work_mem` at 1MB or more, to avoid it.

### Planner settings

All 52 "Query Tuning" settings are accepted with PostgreSQL 18's values,
units, ranges, errors and `pg_settings` rows; [query planning](query-planning.md#planner-settings)
describes which plans they steer.

### Cursor capacity

Each cursor reserves `cursor_bytes` for rows, `cursor_query_bytes` for its query,
parameters with their declared types, and retained input positions, a 1,024-byte
description, and `floor(cursor_bytes / 6)` row positions. Each saved row costs
its data plus six bytes. Long column names can fill the description even when
rows fit; raising `cursor_bytes` does not help there.

- `NO SCROLL` (and forward-only by shape): a top-level forward `FETCH n` or
  `FETCH ALL` from a cursor that reads as you fetch streams its rows to the
  client without saving them, so any count works; other fetches, such as a
  `FETCH` inside a routine, `RELATIVE`/`ABSOLUTE` or one over a row-locking
  cursor, buffer one batch in `cursor_bytes`, which is reused.
- `SCROLL` without volatile functions, unordered and not locking: only the
  current fetch's rows stay in `cursor_bytes`; a top-level forward fetch
  streams, and moving back reads the input again from its first row.
- Other `SCROLL` cursors keep every visited row in `cursor_bytes`, so history
  can exhaust it. When revisited rows are evaluated again, the history holds each row's input
  columns plus 17 bytes of row identity, and a volatile filter adds the rows it
  rejected; the current fetch's output shares the same bytes.
- Ordered cursors save every qualifying row's input columns (plus 17 bytes)
  at the first `FETCH`: the columns the query reads, or every column of a
  row-locking cursor.
- `WITH HOLD`: the unread suffix (plus history when scrolling) must fit; a
  scrollable cursor whose rows are evaluated again saves its whole result.
- A trigger fired inside a writing statement (`INSERT`, `UPDATE`, `DELETE`,
  `MERGE`, `COPY`, `TRUNCATE`, deferred constraints) can read a session
  cursor, including one that locks its rows and one whose data-modifying
  `WITH` queries then run at that first read, as the trigger's own writes do.
  A cursor that calls a user-defined routine for each fetched row raises
  `0A000` there.
- A PL/pgSQL `OPEN` query with data-modifying `WITH` queries runs once, at
  the cursor's first `FETCH` or `MOVE` as in PostgreSQL, and its whole result
  must fit; a cursor closed unread never writes. A full store raises `54000`
  (`cursor result exceeds cursor_bytes`).

Destructive changes to a table an open cursor reads raise `55006`, as in
[PostgreSQL](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/tablecmds.c).
A fetch that failed stays failed after a savepoint rollback (`55000`, `portal
"name" cannot be run`). A PL/pgSQL variable that shadows a source column in an
`OPEN` query raises `42702`. Cursor read behavior is described under
[cursors](#cursors).

## Architecture boundaries and remaining gaps

These are deliberate differences, not unfinished work.

**User example:** an application can receive logically replicated changes, but a
PostgreSQL physical standby cannot replay pos3ql's journal.

- Heap layout, `ctid`, system columns (`xmin`, `xmax`, `cmin`, `cmax`), HOT,
  physical vacuum, physical/streaming replication, hot standby, and binary-WAL
  tools are not provided. Row identities are not exposed as fake heap addresses.
  A query naming one of these columns of a table fails with `0A000` (`system
  column "ctid" is not supported`); as in PostgreSQL, a table column cannot
  take a system column's name (`42701`). The `tableoid` system column is
  provided: it is the oid of the table that holds the row, so
  `SELECT tableoid::regclass, count(*) FROM measurements GROUP BY 1` counts
  the rows of each partition, inheritance child or foreign child, also in
  window queries, in the `FROM` items of `UPDATE ... FROM` and `DELETE
  ... USING`, and in `MERGE` for its target and a table source. For example,
  `MERGE INTO measurements m USING readings r ON m.id = r.id WHEN MATCHED THEN
  UPDATE SET v = r.v RETURNING m.tableoid::regclass` names the partition each
  changed row is in. As in PostgreSQL, a `MERGE` `WHEN` condition may read
  no other system column (`42P10`), and assigning a system column fails with
  `0A000` (`cannot assign to system column "ctid"`).
- PostgreSQL parallel query, JIT, and planner/executor hooks do not exist.
  [Query planning](query-planning.md) describes which plans match
  PostgreSQL's; `SELECT a.id, b.y FROM a LEFT JOIN b ON a.id = b.a_id` over
  analyzed tables prints the same `Hash Right Join` (or merge or nested-loop
  join) as PostgreSQL and returns its rows in the same order. A statement a
  rewrite rule changes shows each statement the rule produces, separated by
  an empty line.
- Logical copies are separate durable databases updated asynchronously; there
  are no shared-storage replicas or multiple writers.
- Monitoring fields for absent subsystems report empty or zero values. A
  session has no memory contexts because every pool is reserved at startup:
  `pg_backend_memory_contexts` is empty, and
  `pg_log_backend_memory_contexts(pid)` returns true for a session's PID after
  logging `logging memory contexts of PID n` and an empty grand total, or warns
  `PID n is not a PostgreSQL server process` and returns false.
- XPath and some definition breadths are smaller subsets, as listed above.
- Compatibility with earlier pos3ql versions is not required; unsupported
  stored formats fail explicitly.
- Regression-schedule omissions are listed explicitly rather than hidden in an
  expected-failure budget.

Unfinished work is tracked in PLAN.md:
[running SQL on several CPU workers](../PLAN.md#run-sql-on-several-cpu-workers)
(SQL currently uses one CPU worker while object-store I/O overlaps),
[pausing every query path](../PLAN.md#pause-and-resume-every-query-path), and
[closing application compatibility gaps](../PLAN.md#close-application-compatibility-gaps).

## Extensions

The SQL extension lifecycle works: control and version scripts, updates,
dependencies, trusted installation, relocation, membership, ownership, comments,
catalogs, transactions, dump/restore, and recovery. Repository fixture packages
test it.

`postgres_fdw` is built in and found in `$system`, the default
`extension_control_path` entry, like PostgreSQL's own contrib packages:
`CREATE EXTENSION postgres_fdw` runs PostgreSQL 18.6's versions 1.0 to 1.2
scripts, which create `postgres_fdw_handler`, `postgres_fdw_validator` and
the `postgres_fdw` wrapper in the target schema. Before that, no handler
exists. Their `LANGUAGE C` functions (`AS '$libdir/postgres_fdw', 'symbol'`)
bind to pos3ql's implementation: a wrapper uses them as its handler and
validator. `postgres_fdw_validator(options, catalog)`,
`postgres_fdw_get_connections()`, `postgres_fdw_disconnect(server)` and
`postgres_fdw_disconnect_all()` work when called; calling the handler fails
with `0A000`.

Like PostgreSQL, pos3ql keeps a postgres_fdw connection open after the
transaction that opened it, one per client connection and user mapping, and
reuses it in later transactions. For example, after `SELECT count(*) FROM
remote_orders`, `SELECT server_name, valid FROM postgres_fdw_get_connections()`
lists the server until `postgres_fdw_disconnect('orders_server')` closes it or
the client disconnects. A server with `keep_connections 'off'` closes its
connection at transaction end, and changing a server or user mapping closes
its connections once no transaction uses them. Unlike PostgreSQL, the number
of open connections across all clients is fixed by `max_foreign_sessions`
(default 16); opening one more fails with `53300 foreign connection cache is
full`. Scans read through a cursor `fetch_size` rows (default 100) at a time,
fetching the next batch only when the query needs more rows: `SELECT * FROM
remote_orders WHERE note LIKE '%x%' LIMIT 1` reads one batch and closes the
cursor. A query that joins a foreign table with other tables, a SQL cursor and
a PL/pgSQL `FOR` loop still read every batch first. An INSERT on a table with
`batch_size` sends its rows in multi-row remote INSERTs, each batch's values
limited to `foreign_send_bytes` (`54000` beyond it). `COPY ... FROM` does the
same when its buffer of up to 1000 rows or 64 KB of input fills and at the
end of the data, as PostgreSQL does; each client connection reserves that
buffer, `foreign_send_bytes` of values, at startup.

A query that reads one foreign table sends the server the work postgres_fdw
would: only the columns it needs, the `WHERE` conditions built from built-in
immutable operators and functions over the table's columns and constants, and
the `ORDER BY` and `LIMIT`/`OFFSET` when they can apply there. For example,
`SELECT id FROM remote_orders WHERE total > 50 AND random() < 0.5 ORDER BY id`
sends `SELECT id, total FROM sales.orders WHERE ((total > 50)) ORDER BY id ASC
NULLS LAST` and checks `random() < 0.5` locally; `EXPLAIN VERBOSE` shows it as
a `Foreign Scan` with that `Remote SQL`. An `UPDATE` or `DELETE` whose
conditions and new values all ship, on a table without triggers, runs as one
remote statement (`Foreign Update`/`Foreign Delete`); otherwise its rows are
read `FOR UPDATE` and each is changed with `... WHERE ctid = $1`. A direct
`UPDATE` or `DELETE` ships the columns its `RETURNING` list reads; one whose
`RETURNING` reads `old` or `new`, or whose table has a row trigger for its event
or a stored generated column, runs row by row (`differential_exact/506`). A
grouped query whose keys and `WHERE` ship is sent as one grouped remote query
(`507`); joins are not sent, so a foreign table joined with other relations is
read whole and joined here, and `EXPLAIN` of such a query fails with `0A000`.
Connection options beyond host, hostaddr, port, dbname, user, password,
connect_timeout, application_name and sslmode fail with `HV00D`; the host must
be a numeric address ([B-351](../BUGS.md)). A failed connection reports
`08001` `could not connect to server "name"` with libpq's message as its
detail (`620`). `CREATE EXTENSION postgres_fdw SCHEMA pg_catalog` fails
because `pg_catalog` is a read-only virtual schema. A `LANGUAGE C` function
of any other library fails with `0A000`; an unknown symbol of
`$libdir/postgres_fdw` fails with `42883`. Other packages in PostgreSQL's
`$system` directory are not available.

Third-party extensions, SQL-only or native, are not certified. PostgreSQL C
libraries, server ABI and hooks, background workers, native procedural
languages, foreign-data wrappers, and custom index callbacks are out of scope;
no native or sandboxed extension ABI is planned.
