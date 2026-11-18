-- A join condition that is an OR of AND arms (clauselist_selectivity_or of
-- each arm's clauselist_selectivity) and the restriction OR each relation
-- takes from it (extract_restriction_or_clauses), which divides the join
-- clause's selectivity (consider_new_or_clause).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/orclauses.c
CREATE TABLE oa883 (id int, k int, x int, y int);
CREATE TABLE ob883 (id int, k int, x int, z int);
INSERT INTO oa883 SELECT g, g % 30, g % 10, g % 7 FROM generate_series(1, 600) g;
INSERT INTO ob883 SELECT g, g % 40, g % 12, g % 5 FROM generate_series(1, 500) g;

ANALYZE oa883;
ANALYZE ob883;
EXPLAIN SELECT oa883.id, ob883.id FROM oa883 JOIN ob883 ON oa883.k = ob883.k WHERE (oa883.x = 1 AND ob883.x = 2) OR (oa883.x = 3 AND ob883.x = 4);
SELECT oa883.id, ob883.id FROM oa883 JOIN ob883 ON oa883.k = ob883.k WHERE (oa883.x = 1 AND ob883.x = 2) OR (oa883.x = 3 AND ob883.x = 4);
EXPLAIN SELECT oa883.id, ob883.id FROM oa883 JOIN ob883 ON oa883.k = ob883.k AND ((oa883.y = 1 AND ob883.z = 2) OR oa883.y = 5);
SELECT count(*), sum(oa883.id), sum(ob883.id) FROM oa883 JOIN ob883 ON oa883.k = ob883.k AND ((oa883.y = 1 AND ob883.z = 2) OR oa883.y = 5);
EXPLAIN SELECT oa883.id, ob883.id FROM oa883 JOIN ob883 ON oa883.k = ob883.k WHERE (oa883.x = 1 AND ob883.z = 2) OR (oa883.y = 3 AND ob883.x < 3) OR (oa883.x = 9 AND ob883.x = 9);
SELECT oa883.id, ob883.id FROM oa883 JOIN ob883 ON oa883.k = ob883.k WHERE (oa883.x = 1 AND ob883.z = 2) OR (oa883.y = 3 AND ob883.x < 3) OR (oa883.x = 9 AND ob883.x = 9);
EXPLAIN SELECT oa883.id, ob883.id FROM oa883 LEFT JOIN ob883 ON oa883.k = ob883.k AND ((oa883.x = 1 AND ob883.x = 1) OR (oa883.x = 2 AND ob883.x = 2)) WHERE oa883.id < 40;
SELECT oa883.id, ob883.id FROM oa883 LEFT JOIN ob883 ON oa883.k = ob883.k AND ((oa883.x = 1 AND ob883.x = 1) OR (oa883.x = 2 AND ob883.x = 2)) WHERE oa883.id < 40;
EXPLAIN SELECT oa883.id, ob883.id FROM oa883 JOIN ob883 ON oa883.k = ob883.k WHERE (oa883.x > 0 AND ob883.z = 1) OR (oa883.x > 1 AND ob883.z = 2);
SELECT count(*), sum(oa883.id), sum(ob883.id) FROM oa883 JOIN ob883 ON oa883.k = ob883.k WHERE (oa883.x > 0 AND ob883.z = 1) OR (oa883.x > 1 AND ob883.z = 2);
