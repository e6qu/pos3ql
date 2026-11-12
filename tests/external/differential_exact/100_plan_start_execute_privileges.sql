-- EXECUTE is checked on every function a plan calls when its executor
-- starts, user-defined or built-in, even when no row reaches the call:
-- select lists, filters, ordering, windows, subqueries, row comparisons,
-- INSERT, ON CONFLICT, MERGE and RETURNING. Operator and cast functions
-- such as texteq are pg_proc rows that can be revoked and called by name.
CREATE ROLE px_caller;
CREATE TABLE px_t(a integer PRIMARY KEY, b text);
CREATE TABLE px_src(a integer, b text);
INSERT INTO px_src VALUES (1, 'x');
GRANT ALL ON px_t, px_src TO px_caller;
CREATE FUNCTION px_sql(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION px_plpgsql(text) RETURNS boolean LANGUAGE plpgsql
  AS 'BEGIN RETURN true; END';
REVOKE EXECUTE ON FUNCTION px_sql(integer), px_plpgsql(text) FROM PUBLIC;
SET ROLE px_caller;
SELECT px_sql(a) FROM px_t;
SELECT a FROM px_t WHERE px_plpgsql(b);
SELECT a FROM px_t ORDER BY px_sql(a);
SELECT 1 FROM px_t LIMIT px_sql(1);
SELECT px_sql(1) WHERE false;
SELECT 1 WHERE false AND px_plpgsql('x');
SELECT CASE WHEN false THEN px_sql(1) END;
SELECT (SELECT px_sql(1)) FROM px_t;
SELECT a FROM px_t WHERE a IN (SELECT px_sql(a) FROM px_src);
SELECT count(*) FILTER (WHERE px_plpgsql(b)) FROM px_t;
SELECT a FROM px_t UNION SELECT px_sql(a) FROM px_t;
INSERT INTO px_t SELECT px_sql(a), b FROM px_t;
INSERT INTO px_t VALUES (1, 'x') RETURNING px_sql(a);
INSERT INTO px_t SELECT a, b FROM px_t
  ON CONFLICT (a) DO UPDATE SET b = 'y' WHERE px_plpgsql(px_t.b);
INSERT INTO px_t SELECT a, b FROM px_t
  ON CONFLICT (a) DO UPDATE SET a = px_sql(1);
UPDATE px_t SET a = px_sql(a);
UPDATE px_t SET b = 'q' RETURNING px_sql(a);
DELETE FROM px_t WHERE px_sql(a) = 1;
DELETE FROM px_t RETURNING px_plpgsql(b);
MERGE INTO px_t USING px_src ON px_t.a = px_src.a
  WHEN MATCHED AND px_plpgsql(px_t.b) THEN DO NOTHING;
MERGE INTO px_t USING px_src ON px_t.a = px_src.a
  WHEN NOT MATCHED THEN INSERT VALUES (px_sql(1), 'z');
MERGE INTO px_t USING px_src ON px_t.a = px_sql(px_src.a)
  WHEN MATCHED THEN DELETE;
RESET ROLE;
REVOKE EXECUTE ON FUNCTION texteq(text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION int4eq(integer, integer) FROM PUBLIC;
SET ROLE px_caller;
SELECT row_number() OVER (PARTITION BY b) FROM px_t;
SELECT row_number() OVER (ORDER BY b) FROM px_src;
SELECT sum(a) OVER (ORDER BY b ROWS 1 PRECEDING) FROM px_t;
SELECT row_number() OVER w FROM px_t WINDOW w AS (PARTITION BY a);
SELECT (a, b) = (1, 'x') FROM px_t;
SELECT ROW(a, b) <> ROW(1, 'x') FROM px_t;
SELECT (a, b) < (1, 'x') FROM px_t;
SELECT count(*) FROM px_t GROUP BY b;
SELECT 1 FROM px_t ORDER BY b;
UPDATE px_t SET b = 'y' WHERE b = 'x';
SELECT texteq('a', 'b');
RESET ROLE;
SELECT texteq('a', 'b'), texteq('a'::varchar, 'a'), int4eq(1, 1);
GRANT EXECUTE ON FUNCTION texteq(text, text), int4eq(integer, integer) TO PUBLIC;
-- Operator, cast and input functions run by name.
SELECT int4um(5), int4up(3), float8um(2.5), numeric_uminus(2.5), int4abs(-3);
SELECT int4(2.5), int8(3), int4in('5'), textcat('a', 'b');
SELECT like('ab', 'a%'), notlike('ab', 'a%'), bpchareq('a', 'a ');
SELECT textregexeq('abc', 'b'), date_eq('2020-01-01', '2020-01-01');
SELECT 'a'::bpchar = 'a '::bpchar, length('a '::bpchar), 'a '::bpchar || '|';
SELECT p.oid::regprocedure, p.proname, p.prosrc, p.proacl
  FROM pg_proc AS p
 WHERE p.oid IN ('texteq(text,text)'::regprocedure, 'int4um(integer)'::regprocedure,
                 'textlike(text,text)'::regprocedure, 'int4in'::regproc)
 ORDER BY p.oid;
SELECT count(*) FROM pg_proc WHERE oid < 16384;
SELECT obj_description('texteq(text,text)'::regprocedure, 'pg_proc');
DROP TABLE px_t, px_src;
DROP FUNCTION px_sql(integer), px_plpgsql(text);
DROP ROLE px_caller;
