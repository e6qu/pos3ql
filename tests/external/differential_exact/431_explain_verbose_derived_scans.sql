-- PostgreSQL 18 EXPLAIN of derived scans: a derived table's, a pulled-up
-- VALUES list's and a function's filter, `VERBOSE` relation schemas and
-- columns qualified by their relation (`"*VALUES*".column1`), an OFFSET's
-- Limit, a HAVING filter, a distinct union's Append and the unique names
-- of a relation a set operation reads again.
CREATE TABLE t431 (a int, b int);
INSERT INTO t431 SELECT g % 10, g FROM generate_series(1, 100) g;
ANALYZE t431;
EXPLAIN (COSTS OFF) SELECT s.a FROM (SELECT a, b FROM t431 ORDER BY b LIMIT 5) s WHERE s.a > 2;
EXPLAIN (COSTS OFF) SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c')) v(x, y) WHERE x > 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c')) v(x, y) WHERE x > 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT y, x FROM (VALUES (1, 'a'), (2, 'b')) v(x, y);
EXPLAIN (COSTS OFF) SELECT * FROM generate_series(1, 10) g WHERE g > 7;
EXPLAIN (COSTS OFF, FORMAT JSON) SELECT * FROM generate_series(1, 10) g;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM t431 WHERE a > 5;
EXPLAIN (VERBOSE, COSTS OFF, FORMAT JSON) SELECT * FROM t431 WHERE a > 5;
EXPLAIN (COSTS OFF) SELECT * FROM t431 OFFSET 3;
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM t431 GROUP BY a HAVING count(*) > 1;
EXPLAIN (COSTS OFF) SELECT a FROM t431 UNION SELECT b FROM t431;
EXPLAIN (COSTS OFF) SELECT a FROM t431 UNION ALL SELECT a FROM t431 UNION ALL SELECT b FROM t431;
CREATE INDEX t431_b ON t431 (b);
SET enable_seqscan = off;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM t431 WHERE b = 5 AND a > 1;
EXPLAIN (COSTS OFF) SELECT * FROM t431 WHERE b = 5 AND a > 1;
RESET enable_seqscan;
