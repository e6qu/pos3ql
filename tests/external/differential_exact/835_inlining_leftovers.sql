-- SQL function inlining of SQL-standard bodies, set-returning bodies with
-- defaults and WITH queries; a window or a coerced column still runs the call.
CREATE TABLE t835 (a int, b text, c int);
INSERT INTO t835 SELECT g, 'x' || g, g % 10 FROM generate_series(1, 2000) g;
CREATE INDEX t835_a ON t835 (a);
ANALYZE t835;
CREATE FUNCTION atomic835(x int) RETURNS int LANGUAGE sql IMMUTABLE
  BEGIN ATOMIC SELECT x * 3; END;
CREATE FUNCTION atomic_rows835(lim int) RETURNS SETOF t835 LANGUAGE sql STABLE
  BEGIN ATOMIC SELECT * FROM t835 WHERE a < lim; END;
CREATE FUNCTION default_rows835(lim int DEFAULT 4) RETURNS SETOF t835 LANGUAGE sql STABLE
  AS 'SELECT * FROM t835 WHERE a < lim';
CREATE FUNCTION with_rows835(lim int) RETURNS TABLE (a int, c int) LANGUAGE sql STABLE
  AS 'WITH w AS (SELECT a, c FROM t835 WHERE a < lim) SELECT a, c FROM w';
CREATE FUNCTION window_rows835(lim int) RETURNS TABLE (a int, n bigint) LANGUAGE sql STABLE
  AS 'SELECT a, count(*) OVER (PARTITION BY c ORDER BY a ROWS BETWEEN lim PRECEDING AND CURRENT ROW) FROM t835 WHERE a < lim';
CREATE FUNCTION coerce_rows835(lim int) RETURNS TABLE (a bigint, b varchar) LANGUAGE sql STABLE
  AS 'SELECT a, b FROM t835 WHERE a < lim';
EXPLAIN SELECT * FROM t835 WHERE atomic835(a) = 9;
EXPLAIN VERBOSE SELECT atomic835(c) FROM t835 WHERE a < 3;
EXPLAIN SELECT * FROM atomic_rows835(3);
EXPLAIN SELECT * FROM default_rows835();
EXPLAIN SELECT * FROM default_rows835(lim => 6);
EXPLAIN SELECT * FROM with_rows835(4);
SELECT atomic835(a) FROM t835 WHERE a < 3 ORDER BY 1;
SELECT * FROM atomic_rows835(3) ORDER BY a;
SELECT * FROM default_rows835() ORDER BY a;
SELECT * FROM with_rows835(4) ORDER BY a;
SELECT * FROM window_rows835(4) ORDER BY a;
SELECT * FROM coerce_rows835(3) ORDER BY a;
