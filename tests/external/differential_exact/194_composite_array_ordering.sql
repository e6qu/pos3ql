-- Arrays of a named composite type compare their elements by record_cmp on
-- the catalog fields: (9,1) < (10,1) and numeric 1.0 = 1.00, in operators,
-- ORDER BY, DISTINCT, GROUP BY, set operations, windows, aggregates and the
-- array search, containment and concatenation functions.
CREATE TYPE ct188 AS (a int, b numeric);
CREATE TABLE t188 (id int, x ct188[]);
INSERT INTO t188 VALUES (1, ARRAY[ROW(10, 1)::ct188]), (2, ARRAY[ROW(9, 1)::ct188]),
  (3, ARRAY[ROW(9, 1.0)::ct188, ROW(1, 2)::ct188]), (4, ARRAY[ROW(9, 1.00)::ct188]);
SELECT id FROM t188 ORDER BY x, id;
SELECT id, x FROM t188 ORDER BY x DESC, id;
SELECT ARRAY[ROW(10, 1)::ct188] < ARRAY[ROW(9, 1)::ct188] AS lt,
  ARRAY[ROW(9, 1.0)::ct188] = ARRAY[ROW(9, 1.00)::ct188] AS eq;
SELECT id, x < ARRAY[ROW(9, 1)::ct188], x > ARRAY[ROW(9, 1)::ct188] FROM t188 ORDER BY id;
SELECT count(DISTINCT x) FROM t188;
SELECT count(*) FROM (SELECT DISTINCT x FROM t188) s;
SELECT count(*) FROM (SELECT x FROM t188 GROUP BY x) s;
SELECT count(*) FROM (SELECT x FROM t188 UNION SELECT x FROM t188) s;
SELECT count(*) FROM (SELECT x FROM t188 INTERSECT SELECT x FROM t188) s;
SELECT count(*) FROM (SELECT x FROM t188 EXCEPT SELECT ARRAY[ROW(9, 1)::ct188]) s;
SELECT id, count(*) OVER (PARTITION BY x), rank() OVER (ORDER BY x) FROM t188 ORDER BY id;
SELECT max(x), min(x) FROM t188;
SELECT array_agg(id ORDER BY x, id) FROM t188;
SELECT greatest(ARRAY[ROW(10, 1)::ct188], ARRAY[ROW(9, 1)::ct188]),
  least(ARRAY[ROW(10, 1)::ct188], ARRAY[ROW(9, 1)::ct188]);
SELECT id FROM t188 WHERE x = ARRAY[ROW(9, 1)::ct188] ORDER BY id;
SELECT id FROM t188 WHERE x IN (ARRAY[ROW(9, 1)::ct188]) ORDER BY id;
SELECT id, x @> ARRAY[ROW(9, 1)::ct188], x && ARRAY[ROW(9, 1)::ct188] FROM t188 ORDER BY id;
SELECT id, ROW(x) < ROW(ARRAY[ROW(9, 1)::ct188]) FROM t188 ORDER BY id;
SELECT nullif(ARRAY[ROW(9, 1)::ct188], ARRAY[ROW(9, 1.0)::ct188]);
SELECT CASE ARRAY[ROW(9, 1)::ct188] WHEN ARRAY[ROW(9, 1.0)::ct188] THEN 'y' ELSE 'n' END;
SELECT array_position(ARRAY[ROW(9, 1.0)::ct188], ROW(9, 1)::ct188),
  array_positions(ARRAY[ROW(9, 1.0)::ct188, ROW(9, 1)::ct188], ROW(9, 1)::ct188);
SELECT array_remove(ARRAY[ROW(9, 1.0)::ct188, ROW(10, 1)::ct188], ROW(9, 1)::ct188);
SELECT array_replace(ARRAY[ROW(9, 1.0)::ct188, ROW(10, 1)::ct188], ROW(9, 1)::ct188, ROW(0, 0)::ct188);
SELECT array_append(ARRAY[ROW(9, 1.0)::ct188], ROW(1, 1)::ct188),
  array_prepend(ROW(1, 1)::ct188, ARRAY[ROW(9, 1.0)::ct188]);
SELECT ARRAY[ROW(9, 1.0)::ct188] || ROW(1, 1)::ct188, ROW(1, 1)::ct188 || ARRAY[ROW(9, 1.0)::ct188];
SELECT array_fill(ROW(1, 2)::ct188, ARRAY[2]);
CREATE INDEX ON t188 (x);
SELECT id FROM t188 WHERE x = ARRAY[ROW(9, 1)::ct188] ORDER BY id;
CREATE DOMAIN dct188 AS ct188;
CREATE TABLE d188 (x dct188[]);
INSERT INTO d188 VALUES (ARRAY[ROW(10, 1)::dct188]), (ARRAY[ROW(9, 1)::dct188]);
SELECT x FROM d188 ORDER BY x;
SELECT x < ARRAY[ROW(9, 2)::dct188] FROM d188 ORDER BY x;
