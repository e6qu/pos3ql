-- DISTINCT over a join (create_final_distinct_paths): a Unique over the
-- rows sorted by its keys, the ORDER BY's first, or a HashAggregate whose
-- rows come back in its simplehash table's order, with the sort and limit
-- above it.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
CREATE TABLE da880 (id int, k int, v int, t text);
CREATE TABLE db880 (id int, k int, w int, t text);
INSERT INTO da880 SELECT g, g % 40, g % 7, 'a' || (g % 11) FROM generate_series(1, 400) g;
INSERT INTO db880 SELECT g, g % 50, g % 3, 'w' || (g % 9) FROM generate_series(1, 300) g;
CREATE INDEX da880_id ON da880 (id);
ANALYZE da880;
ANALYZE db880;
EXPLAIN SELECT DISTINCT da880.v, db880.w FROM da880 JOIN db880 ON da880.k = db880.k;
SELECT DISTINCT da880.v, db880.w FROM da880 JOIN db880 ON da880.k = db880.k;
EXPLAIN SELECT DISTINCT db880.t FROM da880 JOIN db880 ON da880.k = db880.k ORDER BY db880.t DESC;
SELECT DISTINCT db880.t FROM da880 JOIN db880 ON da880.k = db880.k ORDER BY db880.t DESC;
EXPLAIN SELECT DISTINCT da880.v, db880.t FROM da880 JOIN db880 ON da880.k = db880.k ORDER BY db880.t LIMIT 6;
SELECT DISTINCT da880.v, db880.t FROM da880 JOIN db880 ON da880.k = db880.k ORDER BY db880.t LIMIT 6;
EXPLAIN SELECT DISTINCT da880.id, db880.w FROM da880 JOIN db880 ON da880.k = db880.k WHERE da880.id < 100;
SELECT DISTINCT da880.id, db880.w FROM da880 JOIN db880 ON da880.k = db880.k WHERE da880.id < 100;
SET enable_hashagg = off;
EXPLAIN SELECT DISTINCT da880.v, db880.w FROM da880 JOIN db880 ON da880.k = db880.k;
SELECT DISTINCT da880.v, db880.w FROM da880 JOIN db880 ON da880.k = db880.k;
EXPLAIN SELECT DISTINCT db880.w, da880.t FROM da880 JOIN db880 ON da880.k = db880.k ORDER BY db880.w DESC;
SELECT DISTINCT db880.w, da880.t FROM da880 JOIN db880 ON da880.k = db880.k ORDER BY db880.w DESC;
RESET enable_hashagg;
EXPLAIN SELECT DISTINCT da880.t FROM da880 LEFT JOIN db880 ON da880.k = db880.k AND db880.w = 2;
SELECT DISTINCT da880.t FROM da880 LEFT JOIN db880 ON da880.k = db880.k AND db880.w = 2;
