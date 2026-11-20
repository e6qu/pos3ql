-- EXPLAIN of a one-table DISTINCT of expressions, or with an ORDER BY, shows
-- the plan PostgreSQL chooses: a HashAggregate over the computed keys (the
-- ORDER BY items first), then a Sort for the ORDER BY, a Limit above it.
CREATE TABLE x961 (a int, b int, c text);
INSERT INTO x961 SELECT i % 100, i % 7, 'x' || (i % 13) FROM generate_series(1, 2000) i;
ANALYZE x961;
EXPLAIN SELECT DISTINCT a + 1 FROM x961;
EXPLAIN SELECT DISTINCT a % 10, b FROM x961;
EXPLAIN SELECT DISTINCT lower(c) FROM x961;
EXPLAIN SELECT DISTINCT a, b FROM x961 ORDER BY a;
EXPLAIN SELECT DISTINCT a, b FROM x961 ORDER BY a LIMIT 10;
EXPLAIN SELECT DISTINCT a % 7, b FROM x961 ORDER BY b DESC;
EXPLAIN SELECT DISTINCT a + b FROM x961 ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT DISTINCT b, a % 3 FROM x961 ORDER BY 2;
DROP TABLE x961;
