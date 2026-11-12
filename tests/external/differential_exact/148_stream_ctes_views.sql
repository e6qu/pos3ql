-- CTEs and views in a statement that opens a result stream before it runs
-- (a UNION ALL): a CTE read once, or NOT MATERIALIZED and not volatile,
-- inlines as PostgreSQL's planner inlines it, and a view expands under its
-- owner's privileges; a CTE that must run once (MATERIALIZED, read twice,
-- volatile, recursive or data-modifying) keeps the query in the general
-- executor. Results, errors and evaluation order match either way.
CREATE TABLE t (x int, y int, k text);
INSERT INTO t VALUES (0, 1, 'a'), (2, 2, 'b'), (5, 3, 'c');
CREATE TABLE u (x int, z int);
INSERT INTO u VALUES (2, 20), (5, 50);
CREATE SEQUENCE s;
WITH c AS (SELECT 10/x AS q, x FROM t) SELECT x FROM c WHERE x <> 0 UNION ALL SELECT -1;
WITH c AS (SELECT 10/x AS q, x FROM t) SELECT q, x FROM c WHERE x <> 0 UNION ALL SELECT 0, -1;
WITH c AS (SELECT 10/x AS q, x FROM t) SELECT q FROM c UNION ALL SELECT -1;
WITH c(a, b) AS (SELECT x, k FROM t) SELECT * FROM c UNION ALL SELECT 9, 'z';
WITH c(a, b, d) AS (SELECT x, k FROM t) SELECT * FROM c UNION ALL SELECT 9, 'z';
WITH c AS NOT MATERIALIZED (SELECT x, k FROM t WHERE x > 0)
SELECT a.x, b.k FROM c a JOIN c b ON a.x = b.x UNION ALL SELECT 0, 'none';
WITH c AS (SELECT x, k FROM t WHERE x > 0)
SELECT a.x, b.k FROM c a JOIN c b ON a.x = b.x UNION ALL SELECT 0, 'none';
WITH c AS MATERIALIZED (SELECT 10/x AS q, x FROM t) SELECT x FROM c WHERE x <> 0 UNION ALL SELECT -1;
WITH c AS MATERIALIZED (SELECT 10/x AS q, x FROM t WHERE x > 0) SELECT x, q FROM c UNION ALL SELECT -1, 0;
WITH c AS (SELECT nextval('s') AS n, x FROM t) SELECT n, x FROM c UNION ALL SELECT 0, -1;
WITH c AS NOT MATERIALIZED (SELECT nextval('s') AS n, x FROM t) SELECT n, x FROM c UNION ALL SELECT 0, -1;
SELECT currval('s');
WITH c AS (SELECT x FROM t), d AS (SELECT x + 1 AS x FROM c) SELECT x FROM d UNION ALL SELECT -1;
WITH c AS (SELECT x FROM t), d AS (SELECT x FROM c) SELECT c.x, d.x FROM c JOIN d USING (x) UNION ALL SELECT -1, -1;
WITH unused AS (SELECT 1/0 AS n) SELECT x FROM t UNION ALL SELECT -1;
WITH unused AS (SELECT * FROM missing_table) SELECT x FROM t UNION ALL SELECT -1;
WITH RECURSIVE unused AS (SELECT 1/x AS n FROM t UNION ALL SELECT n + 1 FROM unused WHERE n < 3)
SELECT x FROM t UNION ALL SELECT -1;
WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 3)
SELECT n FROM r UNION ALL SELECT -1;
WITH c AS (SELECT x FROM t) SELECT x FROM (WITH d AS MATERIALIZED (SELECT x FROM c) SELECT x FROM d) e
UNION ALL SELECT -1;
WITH ins AS (INSERT INTO u VALUES (7, 70) RETURNING x) SELECT x FROM ins UNION ALL SELECT -1;
SELECT x, z FROM u ORDER BY x;
WITH c AS (SELECT x FROM t) SELECT x FROM c UNION ALL SELECT x FROM c;
WITH c AS (SELECT x, k FROM t) SELECT x, k FROM c WHERE x > 0 UNION ALL SELECT x, k FROM t WHERE x = 0;

CREATE VIEW v AS SELECT 10/x AS q, x, k FROM t;
CREATE VIEW vf AS SELECT x, k FROM t WHERE x > 0;
CREATE VIEW vm AS WITH m AS MATERIALIZED (SELECT x, k FROM t) SELECT x, k FROM m;
SELECT x, k FROM v WHERE x <> 0 UNION ALL SELECT -1, 'none';
SELECT * FROM v UNION ALL SELECT 0, -1, 'none';
SELECT vf.x, u.z FROM vf JOIN u ON u.x = vf.x UNION ALL SELECT -1, -1;
WITH c AS (SELECT x, k FROM vf) SELECT x, k FROM c UNION ALL SELECT -1, 'none';
SELECT x, k FROM vm UNION ALL SELECT -1, 'none';

CREATE ROLE stream_reader;
CREATE ROLE stream_columns;
CREATE ROLE stream_none;
GRANT SELECT ON vf TO stream_reader;
GRANT SELECT ON u TO stream_reader;
GRANT SELECT (x) ON vf TO stream_columns;
SET ROLE stream_reader;
SELECT x, k FROM vf UNION ALL SELECT -1, 'none';
SELECT vf.x, u.z FROM vf JOIN u ON u.x = vf.x UNION ALL SELECT -1, -1;
WITH c AS (SELECT x, k FROM vf) SELECT x, k FROM c UNION ALL SELECT -1, 'none';
SELECT x, k FROM t UNION ALL SELECT -1, 'none';
RESET ROLE;
SET ROLE stream_columns;
SELECT x FROM vf UNION ALL SELECT -1;
SELECT x, k FROM vf UNION ALL SELECT -1, 'none';
RESET ROLE;
SET ROLE stream_none;
SELECT x FROM vf UNION ALL SELECT -1;
WITH c AS (SELECT x FROM vf) SELECT x FROM c UNION ALL SELECT -1;
RESET ROLE;

CREATE VIEW vi WITH (security_invoker = true) AS SELECT x, k FROM t WHERE x > 0;
GRANT SELECT ON vi TO stream_reader;
SET ROLE stream_reader;
SELECT x, k FROM vi UNION ALL SELECT -1, 'none';
RESET ROLE;
GRANT SELECT ON t TO stream_reader;
SET ROLE stream_reader;
SELECT x, k FROM vi UNION ALL SELECT -1, 'none';
RESET ROLE;

CREATE FUNCTION leak(text) RETURNS boolean LANGUAGE plpgsql AS
$$BEGIN RAISE NOTICE 'saw %', $1; RETURN true; END$$;
CREATE VIEW vb WITH (security_barrier) AS SELECT x, k FROM t WHERE x > 2;
SELECT x, k FROM vb WHERE leak(k) UNION ALL SELECT -1, 'none';
SELECT x, k FROM vf WHERE leak(k) UNION ALL SELECT -1, 'none';

ALTER TABLE t ENABLE ROW LEVEL SECURITY;
CREATE POLICY t_reader ON t FOR SELECT TO stream_reader USING (x < 5);
SET ROLE stream_reader;
SELECT x, k FROM vi UNION ALL SELECT -1, 'none';
SELECT x, k FROM vf UNION ALL SELECT -1, 'none';
RESET ROLE;

-- A set operation reads its data-modifying CTEs and calls routines as a
-- SELECT does; CTE column lists longer than the query fail at the name.
SELECT k FROM t WHERE leak(k) UNION ALL SELECT 'z';
SELECT k FROM t WHERE leak(k) UNION SELECT 'z' ORDER BY 1;
WITH ins AS (INSERT INTO u VALUES (8, 80) RETURNING x) SELECT x FROM ins UNION SELECT -1 ORDER BY 1;
WITH c(a, b, d) AS (SELECT x, k FROM t) SELECT * FROM c;
WITH unused(a, b, d) AS (SELECT x, k FROM t) SELECT 1;
WITH c AS (SELECT 10/x AS q, x FROM t), d AS (SELECT * FROM c) SELECT x FROM d WHERE x <> 0 ORDER BY x;
-- Under RECURSIVE a query may read a later one; PostgreSQL analyses it
-- after the queries it reads, and an unreferenced recursive query never runs.
WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT 10/x AS q, x FROM t) SELECT x FROM a WHERE x <> 0 ORDER BY x;
WITH RECURSIVE a AS (SELECT * FROM b), b AS (SELECT * FROM a) SELECT * FROM a;
WITH RECURSIVE r AS (SELECT 1/x AS n FROM t UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT 1;
WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 3), s AS (SELECT n FROM r)
SELECT n FROM s UNION ALL SELECT -1;
