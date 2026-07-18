# Driver-level test: psycopg 3 (a real PostgreSQL driver) against pos3ql.
# psycopg uses the extended query protocol (Parse/Bind/Describe/Execute)
# for parameterized queries.
import psycopg

conn = psycopg.connect(
    host="127.0.0.1", port=5433, user="driver", dbname="postgres",
    sslmode="disable", autocommit=True,
)
cur = conn.cursor()

print("server version reported:", conn.info.server_version)

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

# Aggregates.
cur.execute("SELECT count(*), sum(score) FROM drv")
count, total = cur.fetchone()
assert count == 3 and abs(total - 16.75) < 1e-9, (count, total)
print("aggregates ok:", count, total)

# Errors surface as exceptions with SQLSTATE.
try:
    cur.execute("SELECT 1/0")
    raise AssertionError("expected division by zero")
except psycopg.errors.DivisionByZero as e:
    print("error mapping ok:", e.sqlstate)

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

conn.close()
print("ALL DRIVER TESTS PASSED")
