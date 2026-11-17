-- An operator no built-in or user-defined operator answers fails while the
-- statement is analysed, also when no row is read or the statement is only
-- prepared. Planner folding negates NOT of comparisons, pattern matches,
-- IN lists, AND and OR (negate_clause), a scalar's null test but not a
-- row's, and treats a top-level X = X as X IS NOT NULL; two expressions of
-- one relation compared take the default selectivities.
SELECT 1 %% 2;
PREPARE oa834_mod AS SELECT 1 %% 2;
PREPARE oa834_hash AS SELECT 1 ## 2;
PREPARE oa834_prefix AS SELECT %% 1;
SELECT 1 %% 2 FROM pg_class WHERE false;
CREATE TABLE oa834 (a integer, c integer, t text);
INSERT INTO oa834 SELECT g % 10, g % 7, 'x' || (g % 13) FROM generate_series(1, 1000) g;
INSERT INTO oa834 SELECT NULL, NULL, NULL FROM generate_series(1, 100);
ANALYZE oa834;
SELECT a %% 2 FROM oa834;
SELECT 1 FROM oa834 WHERE a %% 2 = 0;
EXPLAIN SELECT * FROM oa834 WHERE a = a;
EXPLAIN SELECT * FROM oa834 WHERE t = t;
EXPLAIN SELECT * FROM oa834 WHERE a = a AND t = t;
EXPLAIN SELECT * FROM oa834 WHERE a + 1 = a + 1;
EXPLAIN SELECT * FROM oa834 WHERE a = a OR c = 1;
EXPLAIN SELECT * FROM oa834 WHERE NOT (a = a);
EXPLAIN SELECT * FROM oa834 WHERE a <> a;
EXPLAIN SELECT * FROM oa834 WHERE a >= a;
EXPLAIN SELECT * FROM oa834 WHERE a < c;
EXPLAIN SELECT * FROM oa834 WHERE a = c;
EXPLAIN SELECT * FROM oa834 WHERE a + 1 > c * 2;
EXPLAIN SELECT * FROM oa834 WHERE a <> c AND t > 'x5';
EXPLAIN SELECT * FROM oa834 WHERE NOT (a = 3);
EXPLAIN SELECT * FROM oa834 WHERE NOT (a < 3);
EXPLAIN SELECT * FROM oa834 WHERE NOT (a IS NULL);
EXPLAIN SELECT * FROM oa834 WHERE NOT (a IS NOT NULL);
EXPLAIN SELECT * FROM oa834 WHERE NOT (a = 3 AND c = 2);
EXPLAIN SELECT * FROM oa834 WHERE NOT (a = 3 OR c = 2);
EXPLAIN SELECT * FROM oa834 WHERE NOT NOT (a = 3);
EXPLAIN SELECT * FROM oa834 WHERE NOT (t LIKE 'x1%');
EXPLAIN SELECT * FROM oa834 WHERE NOT (a IN (1, 2));
EXPLAIN VERBOSE SELECT NOT (a = 3), NOT (t ILIKE 'x%') FROM oa834;
SELECT count(*) FROM oa834 WHERE NOT (a = 3 OR c IS NULL);
SELECT count(*) FROM oa834 WHERE NOT (NOT (a < 5));
CREATE TYPE oa834_pair AS (x integer, y integer);
CREATE TABLE oa834_rows (id integer, p oa834_pair);
INSERT INTO oa834_rows VALUES (1, ROW(1, NULL)), (2, ROW(1, 2)), (3, NULL), (4, ROW(NULL, NULL));
SELECT id, p IS NULL, p IS NOT NULL, NOT (p IS NULL) FROM oa834_rows ORDER BY id;
SELECT id FROM oa834_rows WHERE NOT (p IS NULL) ORDER BY id;
SELECT id FROM oa834_rows WHERE NOT (p IS NOT NULL) ORDER BY id;
CREATE FUNCTION oa834_add(integer, integer) RETURNS integer LANGUAGE sql AS 'SELECT $1 + $2';
CREATE OPERATOR %% (LEFTARG = integer, RIGHTARG = integer, FUNCTION = oa834_add);
SELECT 1 %% 2;
SELECT a %% 'x'::text FROM oa834;
