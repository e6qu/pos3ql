-- Parenthesized joins as join inputs: deconstruct_jointree reads a nested
-- join's members into the same join problem, so a leading nested join, a
-- comma-separated one and an inner join of inner joins plan as one join
-- search.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/initsplan.c
CREATE TABLE pa884 (id int, k int, v int);
CREATE TABLE pb884 (id int, k int, w int);
CREATE TABLE pc884 (id int, k int, x int);
INSERT INTO pa884 SELECT g, g % 40, g % 7 FROM generate_series(1, 400) g;
INSERT INTO pb884 SELECT g, g % 50, g % 3 FROM generate_series(1, 300) g;
INSERT INTO pc884 SELECT g, g % 30, g % 5 FROM generate_series(1, 200) g;
CREATE INDEX pc884_id ON pc884 (id);
ANALYZE pa884;
ANALYZE pb884;
ANALYZE pc884;
EXPLAIN SELECT pa884.id, pc884.id FROM (pa884 JOIN pb884 ON pa884.k = pb884.k) JOIN pc884 ON pb884.id = pc884.id WHERE pa884.v = 2;
SELECT pa884.id, pc884.id FROM (pa884 JOIN pb884 ON pa884.k = pb884.k) JOIN pc884 ON pb884.id = pc884.id WHERE pa884.v = 2;
EXPLAIN SELECT pa884.id, pc884.id FROM pa884 JOIN (pb884 JOIN pc884 ON pb884.id = pc884.id) ON pa884.k = pb884.k WHERE pc884.x = 1;
SELECT pa884.id, pc884.id FROM pa884 JOIN (pb884 JOIN pc884 ON pb884.id = pc884.id) ON pa884.k = pb884.k WHERE pc884.x = 1;
EXPLAIN SELECT pa884.id, pb884.id FROM pa884, (pb884 LEFT JOIN pc884 ON pb884.id = pc884.id) WHERE pa884.k = pb884.k AND pa884.id < 20;
SELECT pa884.id, pb884.id FROM pa884, (pb884 LEFT JOIN pc884 ON pb884.id = pc884.id) WHERE pa884.k = pb884.k AND pa884.id < 20;
EXPLAIN SELECT * FROM (pa884 LEFT JOIN pb884 ON pa884.k = pb884.k) WHERE pa884.id < 5 ORDER BY pa884.id, pb884.id;
SELECT * FROM (pa884 LEFT JOIN pb884 ON pa884.k = pb884.k) WHERE pa884.id < 5 ORDER BY pa884.id, pb884.id;
