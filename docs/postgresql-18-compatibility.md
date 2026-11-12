# PostgreSQL 18 compatibility

Use this guide to answer "does feature X work, and what are its limits?" before
assuming that a PostgreSQL application or tool works unchanged. Behavior is
checked against actual PostgreSQL at the SQL, SQLSTATE (the five-character
error code), result, catalog, and wire-protocol boundaries. Anything outside the
tested surface must fail with an error rather than being accepted and ignored.

**User example:** `psql`, a driver, and `pg_restore` work on the tested paths.
A PostgreSQL extension that contains a C library cannot be installed, even
though its `CREATE EXTENSION` command is recognized.

The server reports version `18.4 (pos3ql 0.1)`. The vendored PostgreSQL
regression slice is pinned to PostgreSQL 18.6; older test fixtures keep their
exact provenance. [What works today](implemented-baseline.md) describes the
transaction, visibility, permission, cursor, and COPY behavior applications can
rely on.

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

Schemas and catalogs:

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

Identity, inheritance, and partitions:

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
  the parent has a unique index. One difference remains: an equivalent
  `UNIQUE` or `PRIMARY KEY` constraint of the new partition is not attached to
  a parent's plain unique index, so pos3ql builds a separate copy where
  PostgreSQL reuses the constraint's index. Startup refuses (`22000`) a saved
  catalog in which a valid partitioned index lacks its copy on a partition, a
  state earlier pos3ql releases could commit.

Row types:

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

Functions and expressions:

- `pg_get_expr`, `pg_get_constraintdef`, `pg_get_function_arguments` and
  `pg_get_functiondef` print column defaults, CHECK constraints, parameter
  defaults and `RETURN` bodies as PostgreSQL does: `DEFAULT 'x'` on a `text`
  column is shown as `'x'::text`, and `CHECK (v <> '')` on a `varchar` column as
  `CHECK (((v)::text <> ''::text))`. `pg_get_viewdef` uses PostgreSQL's
  indented layout for views over plain tables joined with `ON`. Pretty output
  (`pg_get_constraintdef(oid, true)`, as psql's `\d` uses) drops redundant
  parentheses as PostgreSQL does. Expressions outside the deparser's typed
  coverage and other views keep their stored spelling
  (see [PLAN](../PLAN.md#close-application-compatibility-gaps)).

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
  body accepts only queries, data changes and `CALL`; anything else fails at
  `CREATE` (`0A000`, `COMMIT is not yet supported in unquoted SQL function
  body`). `pg_get_function_sqlbody` returns a `RETURN` or `BEGIN ATOMIC` body
  (`RETURN (x * 2)`; `BEGIN ATOMIC` then one statement per line) and NULL for
  a quoted body; a `SELECT` of expressions without `FROM` is printed in
  PostgreSQL's analyzed form with each column's name (`SELECT (x + 1);`,
  `SELECT abs(x) AS abs;`), other statements as written.
- PL/pgSQL `CALL` assigns output parameters to the variables written as
  their arguments: `CALL doubled(x, y)` updates `x` and `y`, and a constant in
  an output position fails with `42601` (`procedure parameter "b" is an output
  parameter but corresponding argument is not writable`). `EXECUTE 'CALL ...'`
  returns the outputs only through `INTO`. A called procedure may `COMMIT`
  when every caller up to the top-level `CALL` or `DO` could; from a function,
  an explicit transaction block, an exception block, or a `SECURITY DEFINER`
  or `SET` procedure it fails with `2D000` (`invalid transaction
  termination`). `CALL` in a trigger function fails with `0A000`.
- A built-in function cannot implement `CREATE CAST` or `CREATE OPERATOR`
  (`0A000`). Aggregate support functions can name supported built-ins.
- A missing-function message that does not fit the fixed 192-byte error
  message returns `54000`. This includes the signature that `DROP`, `ALTER`,
  `COMMENT ON` and `GRANT` quote for a routine they cannot find, such as
  `function f(integer) does not exist`.
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
  the current database. Changes survive restart and
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
- Assigning an array element (`UPDATE t SET a[5] = 1`, `x[2][1] := 8` or a
  record field's element `r.f[1] := 3` in PL/pgSQL) works as in PostgreSQL: a
  one-dimensional array grows to the subscript and fills the gap with NULLs,
  a NULL array becomes `[5:5]={1}`, and a multidimensional array fails with
  `2202E` outside its bounds. Assigning a slice (`a[1:2] = ...`) fails.
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

Domains and checked inputs:

- Arrays of domains over built-in arrays work (`integer_vector[]` for a domain
  over `integer[]`). Arrays of domains whose base is an array of user-defined
  values are rejected. A domain over an array of scalar domains
  (`CREATE DOMAIN positive_ids AS positive_integer[]`) works.
- `PREPARE q(type, ...)` keeps each parameter's full catalog type, so a
  domain, enum, composite, relation row type or array parameter is coerced by
  `EXECUTE` with PostgreSQL's assignment rules: `EXECUTE q(5)` for an `orders`
  parameter fails with `42804`, and `EXECUTE q(1.5)` for an `integer` one
  passes 2. A parameter the declaration omits takes its type from its use
  (`PREPARE r AS SELECT round($1)` has a `double precision` parameter), and
  one whose use determines nothing is `text`. PostgreSQL instead rejects a
  parameter that stays undetermined inside a call taking `"any"`
  (`PREPARE x AS SELECT pg_typeof($1)` fails with `42P18`).
- Bind validates parameters before function inputs are prepared, so an invalid
  integer parameter fails at Bind. `PREPARE saved AS SELECT
  '{paid}'::checked_status[]` checks the literal once at `PREPARE`;
  `ARRAY['paid']::checked_status[]` checks when executed.
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

Dates and times:

- Time zone abbreviations follow PostgreSQL 18.6's
  [Default set](https://github.com/postgres/postgres/blob/REL_18_6/src/timezone/tznames/Default)
  (checksum-pinned at build) and the session zone: `2021-06-01 12:00 EST` is
  17:00 UTC; with `TimeZone = 'Asia/Shanghai'`, `CST` means UTC+8.
  `timezone_abbreviations` cannot be changed. Named zones need
  `/usr/share/zoneinfo`.
- Numeric time zones are hours east of UTC (`SET timezone TO '2.5'` is
  UTC+02:30), within ±167:59:59; month/day interval fields return `22023`
  ([PostgreSQL rules](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/variable.c#L251),
  [bounds](https://github.com/postgres/postgres/blob/REL_18_6/src/timezone/localtime.c#L667)).
  `NaN` returns `0A000`.

### Statements that cannot yet pause

Some statements must wait for object storage mid-execution. Where pos3ql cannot
yet keep the unfinished work, it fails rather than repeat effects, and rolls
back the statement's SQL writes (issued sequence numbers stay consumed, as in
PostgreSQL). Current `0A000` messages:

| Message | Example cause |
|---|---|
| `expression wait requires an owned invocation log` | A wait inside an assignment, `RETURN`, loop bound, or message expression of a PL/pgSQL function running in retained AFTER-trigger work |
| `SQL row expression requires retained write progress` | A multirow UPDATE that changes one row, then calls a function for the next row that must wait |
| `expression restart requires owned volatile values` | Restarting would repeat an unsaved `random()` or clock value |

Data-change triggers also return `0A000` for a reached literal of an array of a
named domain, such as `PERFORM cardinality('{1}'::positive_integer[])`. Owned
`PERFORM`/`SELECT ... INTO` calls, SQL-language function bodies, and nested
AFTER-trigger results already survive waits. Closing these gaps is tracked in
[PLAN.md](../PLAN.md#pause-and-resume-every-query-path).

## Physical access paths

| Method | Supported paths |
|---|---|
| Btree | Equality, composite leading-prefix/range probes, parameterized joins and DML, ordered and covering scans |
| Hash | Single-key equality, including expression/partial indexes, prepared statements, joins, and DML |
| BRIN | Bitmap equality/range and built-in range/network inclusion; explicit summarization |
| GiST | Built-in network, range/multirange, geometry, and full-text predicates; geometric distance ordering |
| GIN | Array, full-text, JSONB, and jsonpath bitmap predicates |
| SP-GiST | Built-in network, range, geometry, text ordering/prefix, and geometric distance ordering |

Index names describe SQL behavior, not PostgreSQL's page layout; native index
callbacks are not available. MVCC visibility and SQL rechecks decide every
result. [Immutable index navigation](index-navigation.md) specifies pruning,
covering, and nearest-neighbor eligibility.

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
| Identifier length | 63 bytes | `42622` (PostgreSQL truncates with a notice instead) |
| `search_path` | 128 bytes | `22023` |
| `application_name` after PostgreSQL's [ASCII cleaning](https://github.com/postgres/postgres/blob/REL_18_6/src/common/string.c#L81) (`\xhh` escapes) | 64 bytes | `22023`, prior value kept |
| Connection startup option word | 256 bytes | `54000` |
| Timeout settings | 2,147,483,647 ms | `22023`; enabling a disabled-only timeout is `0A000` |
| Simultaneously tracked function calls (`track_functions`) per SQL worker | 1,024 | `54000` |
| Missing-function error text | 192 bytes | `54000` |
| Built-in function NOTICE/WARNING messages raised before the next output message; one message's text | 64; 256 bytes | `54000` |
| Transaction identities and command counters | 32-bit, checked | `54000` before reuse |
| JSON nesting / jsonpath depth | 1,000 / 128 | see [SQL/JSON](sql-json.md) |
| XML nesting, XPath work, constructor output | see [SQL/XML](sql-xml.md) | program-limit error |

### Startup reservations

Each of these is set in the configuration file (see
[operations](operations.md)) and validated before startup. Slot and lease
counts must be 1..4,294,967,295; byte pools must be positive multiples of 64.
Exhaustion returns `54000` naming the pool, before any partial effect.

| Setting(s) | Bounds |
|---|---|
| `sql_arena_bytes` (per-connection statement memory), `work_arena_bytes` (per query workspace, like PostgreSQL's `work_mem`) | Row images, joins, lists, programs, JSON/XML work, table-function rows, sorts and aggregates, retained scan/COPY/validation records (32 bytes per resident row) |
| `max_tables`, `max_extended_statistics`, other catalog `max_*` | One capacity per catalog family; omitting `max_extended_statistics` derives it from `max_tables`, so set it explicitly |
| `max_connections`, `max_prepared_transactions`, `max_subscriptions` | Transaction identity registry has `max_connections + max_prepared_transactions` entries |
| `max_catalog_versions_per_object`, `max_ddl_per_transaction` | Retained definition/undo versions. Enum label images use `max_enums + P` slots, `P = min(max_enums * max_catalog_versions_per_object, (max_connections + max_prepared_transactions + max_subscriptions) * max_ddl_per_transaction)`, each holding `max_enum_labels_per_type` labels |
| `row_history_slots`, `max_row_versions_per_row` | Shared MVCC row history; per-row pending/committed versions. Locking mode requires zero history slots |
| `row_overlay_slots`, `large_object_pages` | Resident row metadata cells |
| `row_work_bytes`, `row_work_leases` | Retained row-ID lists, DML buffers, and loop inputs; fragmentation can block a request despite free bytes |
| `table_metadata_bytes`, `table_metadata_leases` | Schema images and routine input values (starter 64 MiB, 8,192) |
| `statistics_bytes`, `statistics_leases` | Column statistics (starter 1 MiB, 1,024) |
| `index_statistics_slots` | Index counters (starter 4,096) |
| `definition_image_slots` | Schema pins held by executing statements |
| `execution_control_bytes`, `execution_control_leases` | PL/pgSQL source images, locals, loop frames, saved outputs, caught errors |
| `routine_input_slots` | Per-backend routine plan instances |
| `max_prepared`, `prepared_bytes` | Prepared statements per namespace and bytes per statement, shared with bound portals |
| `cursor_bytes`, `cursor_query_bytes` | See [cursors](#cursor-capacity) |
| `conn_send_buffer_bytes`, `result_spool_bytes`, `result_spool_leases` | Result output: the send buffer, then rows queued in the shared spool for a slow client (a private reserve of the send-buffer size holds closing messages). Only a client lagging by more than the spool gets `54000`; the largest single message must fit the send buffer. A paged portal (`Execute` with a row limit) still buffers its result in `portal_result_bytes` |
| `result_stream_bytes` (required; starter 256 KiB, minimum 64 KiB) | Per-connection read position of a large single-table `SELECT` that resumes row by row; a larger estimate spools the result instead, and a stream that still outgrows it fails with `54000` naming the setting |
| `txn_rows`, `copy_transition_bytes` (256 KiB default) | Streamed COPY statement-trigger inputs per connection |
| `commit_queue_slots` | Commits waiting for durability |
| `memtable_bytes` (≤ `u32::MAX`) | Two heap buffers are reserved; reclamation returns `54000` while both are retained |
| `object_store_get_slots`, `object_store_put_slots`, `object_store_block_put_slots`, `wal_upload_buffer_bytes` | Concurrent object-store requests; see [object storage](object-storage.md) |
| `checkpoint_manifest_bytes` and backup/merge/cleanup rosters | Maintenance work per step |

Fixed per-query-workspace limits: 1,024 snapshot and file-owner slots, 64 scan
sources, 64 point-read sources, and 64 copied point reads. At 85% heap use,
writing commands wait for cache reclamation before taking effect; reads,
commits, and cancellation proceed.

### Cursor capacity

Each cursor reserves `cursor_bytes` for rows, `cursor_query_bytes` for its query,
parameters with their declared types, and retained input positions, a 1,024-byte
description, and `floor(cursor_bytes / 6)` row positions. Each saved row costs
its data plus six bytes. Long column names can fill the description even when
rows fit; raising `cursor_bytes` does not help there.

- `NO SCROLL`: `cursor_bytes` bounds one fetched batch and is reused.
- `SCROLL`: every visited row stays in `cursor_bytes`, so history can exhaust it.
  When revisited rows are evaluated again, the history holds each row's input
  columns plus 17 bytes of row identity, and a volatile filter adds the rows it
  rejected; the current fetch's output shares the same bytes.
- Ordered row-locking cursors save every qualifying row's input columns (plus
  17 bytes) at the first `FETCH`.
- `WITH HOLD`: the unread suffix (plus history when scrolling) must fit; a
  scrollable cursor whose rows are evaluated again saves its whole result.

Destructive changes to a table an open cursor reads raise `55006`, as in
[PostgreSQL](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/tablecmds.c).
A fetch that failed stays failed after a savepoint rollback (`55000`, `portal
"name" cannot be run`). A PL/pgSQL variable that shadows a source column in an
`OPEN` query raises `42702`. Cursor read behavior is described in
[What works today](implemented-baseline.md#reads-keep-their-selected-data).

## Architecture boundaries and remaining gaps

These are deliberate differences, not unfinished work.

**User example:** an application can receive logically replicated changes, but a
PostgreSQL physical standby cannot replay pos3ql's journal.

- Heap layout, `ctid`, system columns (`xmin`, `xmax`, `cmin`, `cmax`), HOT,
  physical vacuum, physical/streaming replication, hot standby, and binary-WAL
  tools are not provided. Row identities are not exposed as fake heap addresses.
- PostgreSQL parallel query, JIT, and planner/executor hooks do not exist.
  Cost estimates and `EXPLAIN` text differ from PostgreSQL.
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

Third-party extensions, SQL-only or native, are not certified. PostgreSQL C
libraries, server ABI and hooks, background workers, native procedural
languages, foreign-data wrappers, and custom index callbacks are out of scope;
no native or sandboxed extension ABI is planned.
