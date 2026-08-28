# Driver-level test: psycopg 3 (a real PostgreSQL driver) against pos3ql.
# psycopg uses the extended query protocol (Parse/Bind/Describe/Execute)
# for parameterized queries.
import psycopg

conn = psycopg.connect(
    host="127.0.0.1", port=5433, user="postgres", dbname="postgres",
    sslmode="disable", autocommit=True,
)
cur = conn.cursor()

print("server version reported:", conn.info.server_version)

# Session defaults and current characteristics cross separate extended-query
# messages without collapsing into one implicit configuration state.
cur.execute(
    "SET SESSION CHARACTERISTICS AS TRANSACTION "
    "ISOLATION LEVEL SERIALIZABLE, READ ONLY, DEFERRABLE"
)
cur.execute(
    "SELECT current_setting('default_transaction_isolation'), "
    "current_setting('transaction_isolation'), "
    "current_setting('transaction_read_only'), "
    "current_setting('transaction_deferrable')"
)
assert cur.fetchone() == ("serializable", "serializable", "on", "on")
cur.execute("BEGIN")
cur.execute("SET TRANSACTION READ WRITE, NOT DEFERRABLE")
cur.execute(
    "SELECT current_setting('transaction_isolation'), "
    "current_setting('transaction_read_only'), "
    "current_setting('transaction_deferrable')"
)
assert cur.fetchone() == ("serializable", "off", "off")
cur.execute("ROLLBACK")
cur.execute("RESET ALL")
print("transaction configuration extended protocol ok")

cur.execute("DROP TABLE IF EXISTS drv")
cur.execute("CREATE TABLE drv (id int NOT NULL, name text, score float8)")

# Parameterized inserts — extended protocol with binds.
for row in [(1, "ada", 9.5), (2, "bob", 7.25), (3, "cyd", None)]:
    cur.execute("INSERT INTO drv VALUES (%s, %s, %s)", row)

# Parameterized select.
cur.execute("SELECT name, score FROM drv WHERE id <= %s ORDER BY id", (2,))
rows = cur.fetchall()
assert rows == [("ada", 9.5), ("bob", 7.25)], rows
print("param select ok:", rows)

# Column metadata via Describe.
cur.execute("SELECT id, name FROM drv ORDER BY id LIMIT 1")
names = [d.name for d in cur.description]
assert names == ["id", "name"], names
print("describe ok:", names)

# User-defined cast/operator resolution crosses Parse, binary Bind, Describe,
# and Result as one typed contract.
cur.execute("CREATE TYPE drv_mood AS ENUM ('low', 'high')")
cur.execute(
    "CREATE FUNCTION drv_mood_text(drv_mood) RETURNS text LANGUAGE SQL "
    "RETURN CASE WHEN $1 = 'low' THEN 'low-driver' ELSE 'high-driver' END"
)
cur.execute(
    "CREATE CAST (drv_mood AS text) WITH FUNCTION drv_mood_text(drv_mood)"
)
cur.execute(
    "CREATE FUNCTION drv_catalog_operator(integer, integer) RETURNS text "
    "LANGUAGE SQL RETURN 'driver'"
)
cur.execute(
    "CREATE OPERATOR public.@+ (FUNCTION = drv_catalog_operator, "
    "LEFTARG = integer, RIGHTARG = integer)"
)
cur.execute(
    "CREATE FUNCTION drv_prefix_operator(integer) RETURNS integer "
    "LANGUAGE SQL RETURN -$1"
)
cur.execute(
    "CREATE OPERATOR public.!! (FUNCTION = drv_prefix_operator, RIGHTARG = integer)"
)
cur.execute(
    "SELECT %s::drv_mood::text, %s OPERATOR(public.@+) %s, OPERATOR(public.!!) %s",
    ("low", 7, 9, 11),
)
assert [column.type_code for column in cur.description] == [25, 25, 23], cur.description
assert cur.fetchone() == ("low-driver", "driver", -11)
bcur = conn.cursor(binary=True)
bcur.execute("SELECT %s OPERATOR(public.@+) %s, !! %s", (11, 13, 17))
assert [column.type_code for column in bcur.description] == [25, 23]
assert bcur.fetchone() == ("driver", -17)
bcur.close()
print("cast/operator extended protocol ok")

# Aggregates.
cur.execute("SELECT count(*), sum(score) FROM drv")
count, total = cur.fetchone()
assert count == 3 and abs(total - 16.75) < 1e-9, (count, total)
print("aggregates ok:", count, total)

# Stored PL/pgSQL procedure execution through a parameterized CALL.
cur.execute("CREATE TABLE drv_plpgsql_log(value integer)")
cur.execute(
    "CREATE PROCEDURE drv_plpgsql(value integer) LANGUAGE plpgsql "
    "AS 'BEGIN INSERT INTO drv_plpgsql_log VALUES (value + 1); END'"
)
cur.execute("CALL drv_plpgsql(%s)", (41,))
cur.execute("SELECT value FROM drv_plpgsql_log")
assert cur.fetchone() == (42,)
print("plpgsql procedure ok")

# A lone extended-protocol CALL in autocommit mode is non-atomic: PostgreSQL
# permits COMMIT/ROLLBACK in the procedure and immediately starts the next
# transaction while preserving parameters and local execution state.
cur.execute(
    "CREATE PROCEDURE drv_plpgsql_transaction(value integer) LANGUAGE plpgsql "
    "AS 'BEGIN INSERT INTO drv_plpgsql_log VALUES (value); COMMIT; "
    "INSERT INTO drv_plpgsql_log VALUES (value + 1); END'"
)
cur.execute("CALL drv_plpgsql_transaction(%s)", (50,))
cur.execute("SELECT value FROM drv_plpgsql_log WHERE value >= 50 ORDER BY value")
assert cur.fetchall() == [(50,), (51,)]

transaction_conn = psycopg.connect(
    host="127.0.0.1", port=5433, user="postgres", dbname="postgres",
    sslmode="disable", autocommit=False,
)
transaction_cur = transaction_conn.cursor()
try:
    transaction_cur.execute("CALL drv_plpgsql_transaction(%s)", (60,))
    raise AssertionError("expected invalid transaction termination")
except psycopg.errors.InvalidTransactionTermination as e:
    assert e.sqlstate == "2D000", e.sqlstate
transaction_conn.rollback()
transaction_conn.close()
cur.execute("SELECT count(*) FROM drv_plpgsql_log WHERE value >= 60")
assert cur.fetchone() == (0,)
print("plpgsql transaction control extended protocol ok")

# Errors surface as exceptions with SQLSTATE.
try:
    cur.execute("SELECT 1/0")
    raise AssertionError("expected division by zero")
except psycopg.errors.DivisionByZero as e:
    print("error mapping ok:", e.sqlstate)

# Deferred obligations created by an extended-protocol Bind are checked by
# COMMIT, and a failed commit rolls the transaction back completely.
cur.execute("DROP TABLE IF EXISTS drv_deferred")
cur.execute(
    "CREATE TABLE drv_deferred (value integer, "
    "CONSTRAINT drv_deferred_key UNIQUE (value) DEFERRABLE INITIALLY DEFERRED)"
)
cur.execute("INSERT INTO drv_deferred VALUES (1)")
cur.execute("BEGIN")
cur.execute("INSERT INTO drv_deferred VALUES (%s)", (1,))
try:
    cur.execute("COMMIT")
    raise AssertionError("expected deferred unique violation")
except psycopg.errors.UniqueViolation as e:
    assert e.sqlstate == "23505", e.sqlstate
cur.execute("SELECT count(*) FROM drv_deferred")
assert cur.fetchone() == (1,)
print("deferred constraint extended protocol ok")

# A named constraint-trigger event completed by SET CONSTRAINTS becomes
# pending again when the driver rolls back to the savepoint that preceded it.
cur.execute("CREATE TABLE drv_constraint_trigger_target (id integer PRIMARY KEY)")
cur.execute("CREATE TABLE drv_constraint_trigger_audit (id integer)")
cur.execute(
    "CREATE FUNCTION drv_constraint_trigger_fn() RETURNS trigger LANGUAGE plpgsql AS "
    "'BEGIN INSERT INTO drv_constraint_trigger_audit VALUES (NEW.id); RETURN NEW; END'"
)
cur.execute(
    "CREATE CONSTRAINT TRIGGER drv_constraint_trigger AFTER INSERT "
    "ON drv_constraint_trigger_target DEFERRABLE INITIALLY DEFERRED "
    "FOR EACH ROW EXECUTE FUNCTION drv_constraint_trigger_fn()"
)
cur.execute("BEGIN")
cur.execute("INSERT INTO drv_constraint_trigger_target VALUES (%s)", (7,))
cur.execute("SAVEPOINT queued_trigger")
cur.execute("SET CONSTRAINTS drv_constraint_trigger IMMEDIATE")
cur.execute("SELECT id FROM drv_constraint_trigger_audit")
assert cur.fetchone() == (7,)
cur.execute("ROLLBACK TO SAVEPOINT queued_trigger")
cur.execute("SELECT count(*) FROM drv_constraint_trigger_audit")
assert cur.fetchone() == (0,)
cur.execute("COMMIT")
cur.execute("SELECT id FROM drv_constraint_trigger_audit")
assert cur.fetchone() == (7,)
print("constraint trigger savepoint extended protocol ok")

# NULL parameter handling.
cur.execute("INSERT INTO drv VALUES (%s, %s, %s)", (4, None, 1.0))
cur.execute("SELECT name IS NULL FROM drv WHERE id = %s", (4,))
assert cur.fetchone()[0] is True
print("null params ok")

# UPDATE/DELETE through the driver.
cur.execute("UPDATE drv SET score = score + %s WHERE id = %s", (0.75, 2))
cur.execute("SELECT score FROM drv WHERE id = 2")
assert cur.fetchone()[0] == 8.0
cur.execute("DELETE FROM drv WHERE id = %s", (1,))
cur.execute("SELECT count(*) FROM drv")
assert cur.fetchone()[0] == 3
print("update/delete ok")

# Type schema moves retain the type OID and every existing column reference.
cur.execute("CREATE SCHEMA drv_moved_types")
cur.execute("CREATE TYPE drv_state AS ENUM ('ready', 'blocked')")
cur.execute("CREATE TYPE drv_point AS (x int, y int)")
cur.execute("CREATE TABLE drv_moved_values (state drv_state, point drv_point)")
cur.execute(
    "INSERT INTO drv_moved_values VALUES (%s::drv_state, ROW(%s, %s)::drv_point)",
    ("ready", 3, 4),
)
cur.execute("ALTER TYPE drv_state SET SCHEMA drv_moved_types")
cur.execute("ALTER TYPE drv_point SET SCHEMA drv_moved_types")
cur.execute("SELECT state::text, (point).x, (point).y FROM drv_moved_values")
assert cur.fetchone() == ("ready", 3, 4)
cur.execute("ALTER TYPE drv_moved_types.drv_point ADD ATTRIBUTE code varchar(5) COLLATE \"C\"")
cur.execute("ALTER TYPE drv_moved_types.drv_point ADD ATTRIBUTE label text COLLATE \"C\"")
print("type schema moves extended protocol ok")

# PostgreSQL views become writable through row-level INSTEAD OF triggers. This
# uses Parse/Bind/Describe/Execute for each parameterized DML statement.
cur.execute("DROP TABLE IF EXISTS drv_view_base")
cur.execute("CREATE TABLE drv_view_base (id int PRIMARY KEY, value int)")
cur.execute("INSERT INTO drv_view_base VALUES (1, 10), (2, 20)")
cur.execute("CREATE VIEW drv_view AS SELECT id, value FROM drv_view_base")
cur.execute(
    """
    CREATE FUNCTION drv_view_write() RETURNS trigger LANGUAGE plpgsql AS
    'BEGIN
       IF TG_OP = ''INSERT'' THEN
         INSERT INTO drv_view_base VALUES (NEW.id, NEW.value); RETURN NEW;
       ELSIF TG_OP = ''UPDATE'' THEN
         UPDATE drv_view_base SET value = NEW.value WHERE id = OLD.id; RETURN NEW;
       END IF;
       DELETE FROM drv_view_base WHERE id = OLD.id; RETURN OLD;
     END'
    """
)
cur.execute(
    "CREATE TRIGGER drv_view_write INSTEAD OF INSERT OR UPDATE OR DELETE ON drv_view "
    "FOR EACH ROW EXECUTE FUNCTION drv_view_write()"
)
cur.execute("INSERT INTO drv_view VALUES (%s, %s) RETURNING id, value", (3, 30))
assert cur.fetchone() == (3, 30)
cur.execute(
    "INSERT INTO drv_view (value, id) "
    "SELECT value, id FROM (VALUES (%s, %s)) supplied(value, id) "
    "RETURNING id, value",
    (40, 4),
)
assert cur.fetchone() == (4, 40)
cur.execute("UPDATE drv_view SET value = %s WHERE id = %s RETURNING id, value", (21, 2))
assert cur.fetchone() == (2, 21)
cur.execute("DELETE FROM drv_view WHERE id = %s RETURNING id", (1,))
assert cur.fetchone() == (1,)
cur.execute("CREATE TABLE drv_view_source (id int PRIMARY KEY, value int)")
cur.execute("INSERT INTO drv_view_source VALUES (%s, %s), (%s, %s)", (2, 200, 4, 400))
cur.execute(
    "UPDATE drv_view AS target SET value = source.value FROM drv_view_source AS source "
    "WHERE target.id = source.id RETURNING target.id, target.value"
)
joined_rows = cur.fetchall()
assert sorted(joined_rows) == [(2, 200), (4, 400)], joined_rows
cur.execute(
    "DELETE FROM drv_view AS target USING drv_view_source AS source "
    "WHERE target.id = source.id AND source.id = %s RETURNING id",
    (2,),
)
assert cur.fetchone() == (2,)
cur.execute("SELECT id, value FROM drv_view_base ORDER BY id")
assert cur.fetchall() == [(3, 30), (4, 400)]
print("instead-of view DML extended protocol ok")

# WITH before a data-modifying main statement travels through Parse/Bind/
# Describe/Execute with typed parameters and RETURNING metadata intact.
cur.execute(
    """
    WITH supplied(name, score) AS (
        SELECT %s::text, %s::float8
    )
    INSERT INTO drv
    SELECT %s::int, name, score FROM supplied
    RETURNING id, name, score
    """,
    ("eve", 6.5, 8),
)
assert [d.name for d in cur.description] == ["id", "name", "score"]
assert cur.fetchone() == (8, "eve", 6.5)
print("with dml extended protocol ok")

# A query-local WITH scope keeps its parameter and output metadata through a
# named extended-protocol execution, while materialization evaluates a volatile
# body once even when nested and referenced twice.
cur.execute("CREATE SEQUENCE drv_cte_sequence")
cte_query = """
    WITH outer_value AS MATERIALIZED (
        SELECT nextval('drv_cte_sequence') AS marker, %s::varchar(5) AS label
    )
    SELECT nested.left_marker, nested.right_marker, nested.label
    FROM (
        WITH inner_value AS MATERIALIZED (SELECT marker, label FROM outer_value)
        SELECT left_value.marker AS left_marker,
               right_value.marker AS right_marker,
               left_value.label
        FROM inner_value AS left_value CROSS JOIN inner_value AS right_value
    ) AS nested
"""
cur.execute(cte_query, ("abc",))
assert [d.name for d in cur.description] == ["left_marker", "right_marker", "label"]
assert [d.type_code for d in cur.description] == [20, 20, 1043]
assert cur.fetchone() == (1, 1, "abc")
cur.execute(cte_query, ("xyz",))
assert cur.fetchone() == (2, 2, "xyz")
print("nested materialized CTE extended protocol ok")

# RowDescription atttypmod: a table column carries its declared modifier and a
# cast its target's, while a computed expression carries none — psycopg derives
# display_size/precision/scale from it, so a client sees varchar(5) as 5.
cur.execute("DROP TABLE IF EXISTS drv_typmod")
cur.execute("CREATE TABLE drv_typmod(v varchar(5), n numeric(6,2), t timestamp(3), label text COLLATE \"C\")")
cur.execute("SELECT v, n, t, v::varchar(9), upper(v) FROM drv_typmod")
got = [(d.precision, d.scale, d.display_size) for d in cur.description]
assert got == [
    (None, None, 5),
    (6, 2, None),
    (3, None, None),
    (None, None, 9),
    (None, None, None),
], f"typmod on the wire: {got}"
print("row description typmod ok")

cur.execute("INSERT INTO drv_typmod VALUES ('abc', 1.25, timestamp '2000-01-01', 'label')")
record_queries = [
    "SELECT (ROW(v,label)).f1, (ROW(v,label)).f2 FROM drv_typmod",
    "SELECT (q).f1, (q).f2 FROM (SELECT ROW(v,label) q FROM drv_typmod) s",
    "WITH s AS (SELECT ROW(v,label) q FROM drv_typmod) SELECT (q).* FROM s",
    "SELECT (ROW(1,2,v,label)::drv_moved_types.drv_point).code, "
    "(ROW(1,2,v,label)::drv_moved_types.drv_point).label FROM drv_typmod",
]
for query in record_queries:
    cur.execute(query)
    got = [(d.type_code, d.display_size) for d in cur.description]
    assert got == [(1043, 5), (25, None)], f"record field metadata: {query}: {got}"
    assert cur.fetchone() == ("abc", "label")
bcur = conn.cursor(binary=True)
bcur.execute(record_queries[1])
assert [(d.type_code, d.display_size) for d in bcur.description] == [(1043, 5), (25, None)]
assert bcur.fetchone() == ("abc", "label")
bcur.close()
print("record field metadata ok")

# Catalog overload identity stays semantic while PostgreSQL exposes a domain's
# base representation in RowDescription. A one-column RETURNS TABLE call is a
# scalar set outside FROM, not an expandable record.
cur.execute("CREATE DOMAIN drv_srf_count AS integer CHECK (VALUE > 0)")
cur.execute(
    "CREATE FUNCTION drv_srf_scalar(value drv_srf_count) RETURNS drv_srf_count "
    "LANGUAGE SQL RETURN value"
)
cur.execute(
    "CREATE FUNCTION drv_srf_record(value integer) "
    "RETURNS TABLE(number integer, source text) LANGUAGE SQL AS 'SELECT 1, ''integer'''"
)
cur.execute(
    "CREATE FUNCTION drv_srf_record(value drv_srf_count) "
    "RETURNS TABLE(label text, accepted boolean) LANGUAGE SQL AS 'SELECT ''domain'', true'"
)
cur.execute(
    "CREATE FUNCTION drv_srf_one(value drv_srf_count) "
    "RETURNS TABLE(label text) LANGUAGE SQL AS 'SELECT ''domain'''"
)
cur.execute("CREATE TABLE drv_srf_input(value drv_srf_count)")
cur.execute("INSERT INTO drv_srf_input VALUES (1)")
catalog_srf_query = (
    "SELECT drv_srf_scalar(input.value), (drv_srf_record(input.value)).* "
    "FROM drv_srf_input AS input"
)
cur.execute(catalog_srf_query)
assert [(d.type_code, d.display_size) for d in cur.description] == [
    (23, None),
    (25, None),
    (16, None),
]
assert cur.fetchone() == (1, "domain", True)
bcur = conn.cursor(binary=True)
bcur.execute(catalog_srf_query)
assert [d.type_code for d in bcur.description] == [23, 25, 16]
assert bcur.fetchone() == (1, "domain", True)
bcur.close()
try:
    cur.execute("SELECT (drv_srf_one(1::drv_srf_count)).*")
    raise AssertionError("single-column RETURNS TABLE expanded as a record")
except psycopg.errors.WrongObjectType as error:
    assert error.sqlstate == "42809"
print("catalog SRF typed boundary ok")

# char(n): the blank padding is part of the value on the wire — in both text
# and binary result formats (PostgreSQL's bpchar binary send is the padded
# text bytes) — while length() and equality ignore it.
cur.execute("DROP TABLE IF EXISTS drv_bpchar")
cur.execute("CREATE TABLE drv_bpchar(c char(5))")
cur.execute("INSERT INTO drv_bpchar VALUES ('hi')")
cur.execute("SELECT c, length(c), c = 'hi' FROM drv_bpchar")
assert cur.fetchone() == ("hi   ", 2, True), "bpchar text format"
bcur = conn.cursor(binary=True)
bcur.execute("SELECT c FROM drv_bpchar")
assert bcur.fetchone()[0] == "hi   ", "bpchar binary format"
bcur.close()
print("bpchar wire ok")

# smallint: OID 21, 2-byte binary payload, values intact in both formats.
cur.execute("DROP TABLE IF EXISTS drv_int2")
cur.execute("CREATE TABLE drv_int2(s smallint)")
cur.execute("INSERT INTO drv_int2 VALUES (32767), (-32768)")
cur.execute("SELECT s FROM drv_int2 ORDER BY s")
assert cur.description[0].type_code == 21, f"smallint oid: {cur.description[0].type_code}"
assert [r[0] for r in cur.fetchall()] == [-32768, 32767]
bcur = conn.cursor(binary=True)
bcur.execute("SELECT s FROM drv_int2 ORDER BY s")
assert [r[0] for r in bcur.fetchall()] == [-32768, 32767], "smallint binary format"
bcur.close()
print("smallint wire ok")

# Declarative partitioning must preserve extended-protocol parameter typing and
# RETURNING while physical storage changes leaf.  Updating the key crosses a
# range boundary through the parent.
cur.execute("CREATE TABLE drv_partitioned (id int PRIMARY KEY, note text) PARTITION BY RANGE (id)")
cur.execute("CREATE TABLE drv_partitioned_low PARTITION OF drv_partitioned FOR VALUES FROM (0) TO (10)")
cur.execute("CREATE TABLE drv_partitioned_high PARTITION OF drv_partitioned FOR VALUES FROM (10) TO (20)")
cur.execute(
    "INSERT INTO drv_partitioned VALUES (%s, %s) RETURNING id, note",
    (1, "low"),
)
assert cur.fetchone() == (1, "low")
cur.execute(
    "UPDATE drv_partitioned SET id = %s, note = %s WHERE id = %s RETURNING id, note",
    (11, "high", 1),
)
assert cur.fetchone() == (11, "high")
cur.execute("SELECT id, note FROM drv_partitioned")
assert cur.fetchone() == (11, "high")
print("partitioned extended protocol routing ok")

cur.execute("CREATE TABLE drv_partition_tree (id int, region int) PARTITION BY RANGE (id)")
cur.execute(
    "CREATE TABLE drv_partition_mid PARTITION OF drv_partition_tree "
    "FOR VALUES FROM (0) TO (100) PARTITION BY LIST (region)"
)
cur.execute("CREATE TABLE drv_partition_leaf PARTITION OF drv_partition_mid FOR VALUES IN (1)")
cur.execute("CREATE TABLE drv_partition_other (id int, region int)")
cur.execute("ALTER TABLE drv_partition_mid ATTACH PARTITION drv_partition_other DEFAULT")
cur.executemany("INSERT INTO drv_partition_tree VALUES (%s, %s)", [(10, 1), (20, 2)])
cur.execute("SELECT id, region FROM drv_partition_tree WHERE id = %s", (20,))
assert cur.fetchone() == (20, 2)
cur.execute("ALTER TABLE drv_partition_mid DETACH PARTITION drv_partition_other")
cur.execute("SELECT id, region FROM drv_partition_other")
assert cur.fetchone() == (20, 2)
print("subpartition attach/detach extended protocol ok")
cur.execute("DROP TABLE drv_partition_leaf, drv_partition_other, drv_partition_mid, drv_partition_tree")

# TABLESAMPLE arguments are inferred/coerced at the source boundary, including
# binary extended-protocol values selected by the driver.
cur.execute("CREATE TABLE drv_sample_source (id integer PRIMARY KEY)")
cur.execute("INSERT INTO drv_sample_source SELECT value FROM generate_series(1,20) value")
cur.execute(
    "SELECT count(*) FROM drv_sample_source "
    "TABLESAMPLE BERNOULLI (%s) REPEATABLE (%s)",
    (100.0, 42.0),
)
assert cur.fetchone() == (20,)
bcur = conn.cursor(binary=True)
bcur.execute(
    "SELECT count(*) FROM drv_sample_source "
    "TABLESAMPLE SYSTEM (%s) REPEATABLE (%s)",
    (0.0, 42.0),
)
assert bcur.fetchone() == (0,)
bcur.close()
try:
    cur.execute("SELECT * FROM drv_sample_source TABLESAMPLE BERNOULLI (%s)", (None,))
    raise AssertionError("NULL TABLESAMPLE percentage was accepted")
except psycopg.DatabaseError as error:
    assert error.sqlstate == "2202H", error.sqlstate
print("TABLESAMPLE extended protocol ok")

conn.close()

print("ALL DRIVER TESTS PASSED")
