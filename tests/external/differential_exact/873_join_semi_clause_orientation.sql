-- A semi join to a relation with a unique index: reduce_unique_semijoins
-- runs before the base relations' paths and derives each class's clause
-- from the join's left side to its inner relation, so the inner relation's
-- index paths, which reuse that clause, and every join above print it left
-- side first (generate_join_implied_equalities, create_join_clause).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/analyzejoins.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/equivclass.c
CREATE TABLE sa873 (id int, k int, v int, w text);
CREATE TABLE sb873 (id int, k int, v int, w text);
CREATE TABLE sc873 (id int, k int, v int, w text);
INSERT INTO sa873 SELECT g, CASE WHEN g % 7 = 3 THEN NULL ELSE g % 20 END, (g * 7) % 13, 'a' || (g % 9) FROM generate_series(1, 3) g;
INSERT INTO sb873 SELECT g, CASE WHEN g % 7 = 3 THEN NULL ELSE g % 20 END, (g * 7) % 13, 'b' || (g % 9) FROM generate_series(1, 4) g;
INSERT INTO sc873 SELECT g, CASE WHEN g % 7 = 3 THEN NULL ELSE g % 20 END, (g * 7) % 13, 'c' || (g % 9) FROM generate_series(1, 6) g;
CREATE INDEX sa873_k ON sa873 (k);
CREATE INDEX sb873_k ON sb873 (k);
CREATE UNIQUE INDEX sb873_id ON sb873 (id);
CREATE INDEX sc873_k ON sc873 (k);
CREATE UNIQUE INDEX sc873_id ON sc873 (id);
ANALYZE sa873; ANALYZE sb873; ANALYZE sc873;
EXPLAIN (COSTS OFF) SELECT sa873.id, sa873.w FROM sa873 RIGHT JOIN sb873 ON sa873.k = sb873.v WHERE sb873.k IS NULL AND EXISTS (SELECT 1 FROM sc873 WHERE sc873.k = sa873.id);
SELECT sa873.id, sa873.w FROM sa873 RIGHT JOIN sb873 ON sa873.k = sb873.v WHERE sb873.k IS NULL AND EXISTS (SELECT 1 FROM sc873 WHERE sc873.k = sa873.id) ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT sa873.id, sa873.w FROM sa873 JOIN sb873 ON sa873.k = sb873.v WHERE EXISTS (SELECT 1 FROM sc873 WHERE sc873.k = sa873.id);
EXPLAIN (COSTS OFF) SELECT sa873.id, sa873.w FROM sb873 JOIN sa873 ON sa873.k = sb873.v WHERE sb873.k IS NULL AND sa873.id IN (SELECT sc873.k FROM sc873);
EXPLAIN (COSTS OFF) SELECT sa873.id, sb873.id FROM sa873 LEFT JOIN sb873 ON sa873.k = sb873.k WHERE EXISTS (SELECT 1 FROM sc873 WHERE sc873.k = sb873.v);
SELECT sa873.id, sb873.id FROM sa873 LEFT JOIN sb873 ON sa873.k = sb873.k WHERE EXISTS (SELECT 1 FROM sc873 WHERE sc873.k = sb873.v) ORDER BY 1, 2;
EXPLAIN SELECT sa873.id, sa873.w FROM sa873 JOIN sb873 ON sa873.k = sb873.v WHERE EXISTS (SELECT 1 FROM sc873 WHERE sc873.k = sa873.id);
