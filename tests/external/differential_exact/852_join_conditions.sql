-- Join conditions other than equalities: an inequality of two relations'
-- columns (neqjoinsel: the equality's complement, a semi join's the outer
-- column's non-NULL fraction), comparisons (scalarltjoinsel), and OR
-- conditions whose arms are such comparisons, equalities or conditions of
-- one relation (clauselist_selectivity_or), each evaluated as a join
-- filter; an index scan of the inner relation applies an inequality as a
-- filter it moves in, under a Memoize in binary mode.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/selfuncs.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/clausesel.c
CREATE TABLE oa852 (id int, k int, v int, w text);
CREATE TABLE ob852 (id int, k int, v int, w text);
CREATE TABLE oc852 (id int, k int, v int, w text);
INSERT INTO oa852 SELECT g, g % 50, g % 7, 'a' || g FROM generate_series(1, 400) g;
INSERT INTO ob852 SELECT g, g % 300, g % 11, 'b' || g FROM generate_series(1, 600) g;
INSERT INTO oc852 SELECT g, g % 40, g % 5, 'c' || g FROM generate_series(1, 300) g;
CREATE INDEX ob852_k ON ob852 (k);
ANALYZE oa852; ANALYZE ob852; ANALYZE oc852;
EXPLAIN SELECT oa852.id, oc852.id FROM oa852 JOIN oc852 ON oa852.k = oc852.k AND oa852.v <> oc852.v;
SELECT count(*), sum(oa852.id), sum(oc852.id) FROM oa852 JOIN oc852 ON oa852.k = oc852.k AND oa852.v <> oc852.v;
EXPLAIN SELECT oa852.id, oc852.id FROM oa852 JOIN oc852 ON oa852.k = oc852.k AND (oa852.v = oc852.v OR oa852.id < oc852.id);
SELECT count(*), sum(oa852.id), sum(oc852.id) FROM oa852 JOIN oc852 ON oa852.k = oc852.k AND (oa852.v = oc852.v OR oa852.id < oc852.id);
EXPLAIN SELECT oa852.id, oc852.id FROM oa852 LEFT JOIN oc852 ON oa852.k = oc852.k AND (oa852.v = 3 OR oc852.v = 2);
SELECT count(*), sum(oa852.id), sum(oc852.id) FROM oa852 LEFT JOIN oc852 ON oa852.k = oc852.k AND (oa852.v = 3 OR oc852.v = 2);
EXPLAIN SELECT oa852.id, oc852.id FROM oa852 JOIN oc852 ON oa852.k = oc852.k WHERE oa852.v < 3 OR oc852.v > 3;
SELECT count(*) FROM oa852 JOIN oc852 ON oa852.k = oc852.k WHERE oa852.v < 3 OR oc852.v > 3;
EXPLAIN SELECT oa852.id, ob852.id FROM oa852 LEFT JOIN ob852 ON oa852.k = ob852.k AND oa852.v <> ob852.v WHERE oa852.id < 5;
SELECT oa852.id, ob852.id FROM oa852 LEFT JOIN ob852 ON oa852.k = ob852.k AND oa852.v <> ob852.v WHERE oa852.id < 5 ORDER BY 1, 2;
EXPLAIN SELECT oa852.id, oc852.id FROM oa852 JOIN oc852 ON oa852.v <> oc852.v WHERE oa852.id < 3 AND oc852.id < 5;
EXPLAIN SELECT oa852.id FROM oa852 WHERE EXISTS (SELECT 1 FROM oc852 WHERE oc852.k = oa852.k AND oc852.v <> oa852.v);
SELECT oa852.id FROM oa852 WHERE EXISTS (SELECT 1 FROM oc852 WHERE oc852.k = oa852.k AND oc852.v <> oa852.v) ORDER BY 1 LIMIT 10;
EXPLAIN SELECT oa852.id FROM oa852 WHERE NOT EXISTS (SELECT 1 FROM oc852 WHERE oc852.k = oa852.k AND oc852.v <> oa852.v);
EXPLAIN SELECT oa852.id, oc852.id FROM oa852 LEFT JOIN oc852 ON oa852.k = oc852.k AND (oa852.v < oc852.v OR oc852.id = 7) WHERE oa852.id < 30;
SELECT count(*), sum(oc852.id) FROM oa852 LEFT JOIN oc852 ON oa852.k = oc852.k AND (oa852.v < oc852.v OR oc852.id = 7) WHERE oa852.id < 30;
