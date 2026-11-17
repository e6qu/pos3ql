-- Parameterized index scans of a join's relation that also filter by the
-- other join clauses movable into them (get_baserel_parampathinfo): an
-- outer join's equalities and comparisons of columns no index applies,
-- several clauses of one key column, a clause reading a column an outer
-- join nulls (the path then waits for that join), and a Memoize cache over
-- such a scan, in binary mode when a moved clause is not a hash equality
-- (paraminfo_get_equal_hashops). A key column's NULL entries share one
-- posting list at the end of the index (_bt_buildadd).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/relnode.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/indxpath.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/joinpath.c
CREATE TABLE pa849 (id int, k int, v int, w text);
CREATE TABLE pb849 (id int, k int, v int, w text);
CREATE TABLE pc849 (id int, k int, v int, w text);
CREATE TABLE pd849 (id int, k int, v int, w text);
INSERT INTO pa849 SELECT g, g % 50, g % 7, 'a' || g FROM generate_series(1, 400) g;
INSERT INTO pb849 SELECT g, g % 300, g % 11, 'b' || g FROM generate_series(1, 1200) g;
INSERT INTO pc849 SELECT g, g % 40, g % 5, 'c' || g FROM generate_series(1, 300) g;
INSERT INTO pd849 SELECT g, CASE WHEN g % 11 = 3 THEN NULL ELSE g % 10 END, (g * 7) % 13, 'd' || g FROM generate_series(1, 1500) g;
CREATE INDEX pb849_k ON pb849 (k);
CREATE UNIQUE INDEX pc849_id ON pc849 (id);
CREATE INDEX pd849_k ON pd849 (k);
ANALYZE pa849; ANALYZE pb849; ANALYZE pc849; ANALYZE pd849;
EXPLAIN SELECT pa849.id, pb849.id FROM pa849 LEFT JOIN pb849 ON pa849.k = pb849.k AND pa849.v = pb849.v WHERE pa849.id < 3;
SELECT pa849.id, pb849.id FROM pa849 LEFT JOIN pb849 ON pa849.k = pb849.k AND pa849.v = pb849.v WHERE pa849.id < 20 ORDER BY 1, 2;
EXPLAIN SELECT pa849.id, pb849.id FROM pa849 LEFT JOIN pb849 ON pa849.k = pb849.k AND pa849.id = pb849.k WHERE pa849.id < 3;
SELECT pa849.id, pb849.id FROM pa849 LEFT JOIN pb849 ON pa849.k = pb849.k AND pa849.id = pb849.k WHERE pa849.id < 60 ORDER BY 1, 2;
EXPLAIN SELECT pa849.id, pb849.id, pc849.id FROM pa849 JOIN pc849 ON pa849.id = pc849.id LEFT JOIN pb849 ON pc849.k = pb849.k AND pa849.v = pb849.v WHERE pa849.id < 10;
SELECT pa849.id, pb849.id, pc849.id FROM pa849 JOIN pc849 ON pa849.id = pc849.id LEFT JOIN pb849 ON pc849.k = pb849.k AND pa849.v = pb849.v WHERE pa849.id < 10 ORDER BY 1, 2, 3;
EXPLAIN SELECT pa849.id, pb849.id FROM pa849 LEFT JOIN pb849 ON pa849.k = pb849.k AND pa849.v < pb849.v WHERE pa849.id < 3;
SELECT count(*), sum(pb849.id) FROM pa849 LEFT JOIN pb849 ON pa849.k = pb849.k AND pa849.v < pb849.v WHERE pa849.id < 3;
EXPLAIN SELECT pc849.id, pd849.id, pd849.w FROM pc849 LEFT JOIN pd849 ON pc849.v = pd849.k AND pc849.v < pd849.v;
SELECT count(*), sum(pd849.id) FROM pc849 LEFT JOIN pd849 ON pc849.v = pd849.k AND pc849.v < pd849.v;
EXPLAIN SELECT pa849.id, pc849.id, pb849.id FROM pa849 LEFT JOIN pc849 ON pa849.k = pc849.id LEFT JOIN pb849 ON pc849.k = pb849.k;
SELECT count(*), sum(pb849.id), sum(pc849.id) FROM pa849 LEFT JOIN pc849 ON pa849.k = pc849.id LEFT JOIN pb849 ON pc849.k = pb849.k;
EXPLAIN SELECT pa849.id, pc849.id, pb849.id FROM pa849 FULL JOIN pc849 ON pa849.v = pc849.k LEFT JOIN pb849 ON pc849.id = pb849.k WHERE pb849.v IS NULL;
EXPLAIN SELECT * FROM pd849 ORDER BY k;
