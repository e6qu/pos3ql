-- The planner replaces a call of a SQL function whose body is one plain
-- expression by that expression (`inline_function`), and a set-returning SQL
-- function in FROM by a subquery of its body (`inline_set_returning_function`):
-- the plan prints, costs and matches index keys with the inlined expression,
-- and an index over an inlinable call is an index over its body.
CREATE TABLE t800 (a int, b text, c int);
INSERT INTO t800 SELECT g, 'x' || g, g % 10 FROM generate_series(1, 2000) g;
CREATE FUNCTION dbl800(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 * 2';
CREATE FUNCTION add800(x int, y int DEFAULT 7) RETURNS int LANGUAGE sql AS 'SELECT x + y';
CREATE FUNCTION up800(text) RETURNS text LANGUAGE sql IMMUTABLE STRICT AS 'SELECT upper($1)';
CREATE FUNCTION big800(bigint) RETURNS bigint LANGUAGE sql STABLE AS 'SELECT $1 + 1';
CREATE FUNCTION lt800(x int, y int) RETURNS boolean LANGUAGE sql RETURN x < y;
CREATE FUNCTION rnd800(int) RETURNS float8 LANGUAGE sql IMMUTABLE AS 'SELECT $1 + random()';
CREATE FUNCTION twice800(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 + $1';
CREATE INDEX t800_dbl ON t800 (dbl800(a));
CREATE INDEX t800_up ON t800 (up800(b));
CREATE INDEX t800_plus ON t800 ((a + 0));
ANALYZE t800;
EXPLAIN SELECT * FROM t800 WHERE dbl800(a) = 10;
EXPLAIN SELECT * FROM t800 WHERE a * 2 = 10;
EXPLAIN SELECT * FROM t800 WHERE up800(b) = 'X5';
EXPLAIN SELECT * FROM t800 WHERE upper(b) = 'X5';
EXPLAIN SELECT * FROM t800 WHERE (a + 0) = 5;
EXPLAIN SELECT add800(a, c) FROM t800 WHERE add800(a) > 1990;
EXPLAIN SELECT add800(y => 1, x => a) FROM t800 WHERE c = 1;
EXPLAIN SELECT big800(a) FROM t800 WHERE big800(c) = 3;
EXPLAIN SELECT * FROM t800 WHERE lt800(a, 3);
EXPLAIN SELECT * FROM t800 WHERE rnd800(a) > 5;
EXPLAIN SELECT twice800(a) FROM t800 WHERE twice800(c + 1) = 4;
EXPLAIN SELECT * FROM t800 WHERE a = dbl800(3);
EXPLAIN VERBOSE SELECT dbl800(a), up800(b), big800(c) FROM t800 WHERE a < 3;
SELECT dbl800(a), up800(b), big800(c), add800(a), twice800(a) FROM t800 WHERE lt800(a, 3) ORDER BY 1;
SELECT * FROM t800 WHERE dbl800(a) = 10;
-- Folding an inlined body with constant arguments names the function.
CREATE FUNCTION div800(int) RETURNS int LANGUAGE sql STABLE AS 'SELECT 10 / $1';
EXPLAIN SELECT div800(a) FROM t800 WHERE a = 1;
SELECT div800(0) FROM t800 WHERE false;
-- A set-returning SQL function in FROM becomes a subquery of its body.
CREATE FUNCTION rows800(lim int) RETURNS SETOF t800 LANGUAGE sql STABLE
  AS 'SELECT * FROM t800 WHERE a < lim';
CREATE FUNCTION vals800(int) RETURNS SETOF int LANGUAGE sql STABLE
  AS 'SELECT a FROM t800 WHERE a < $1';
CREATE FUNCTION vrows800(int) RETURNS SETOF t800 LANGUAGE sql VOLATILE
  AS 'SELECT * FROM t800 WHERE a < $1';
CREATE FUNCTION plrows800(int) RETURNS SETOF int LANGUAGE plpgsql
  AS 'BEGIN RETURN QUERY SELECT a FROM t800 WHERE a < $1; END';
EXPLAIN SELECT * FROM rows800(5) r WHERE c > 1;
EXPLAIN SELECT * FROM vals800(5);
EXPLAIN SELECT * FROM vrows800(5);
EXPLAIN SELECT * FROM vrows800(5) WHERE c = 3;
EXPLAIN SELECT * FROM plrows800(5);
SELECT * FROM rows800(4) r ORDER BY a;
SELECT * FROM vals800(3) v;
-- A function scan costs the call once and each of its rows.
EXPLAIN SELECT * FROM generate_series(1, 100) g WHERE g > 50;
EXPLAIN SELECT * FROM generate_series(1, 10, 3);
EXPLAIN SELECT * FROM generate_series(10, 1);
EXPLAIN SELECT * FROM unnest(ARRAY[1, 2, 3]) u;
EXPLAIN SELECT * FROM jsonb_each('{"a": 1}');
DROP TABLE t800 CASCADE;
