-- Parameterized joins in queries with outer joins: nested loops, hash
-- joins and merge joins whose inputs read a relation outside them
-- (calc_nestloop_required_outer, calc_non_nestloop_required_outer), each
-- with the join clauses its parameterization moves into it
-- (get_joinrel_parampathinfo) and its ppi_rows, are weighed where an outer
-- join allows them (param_source_rels); an index scan's moved clause may
-- read a column an outer join nulls, the path then requiring that join.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/joinpath.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/relnode.c
CREATE TABLE qa850 (id int, k int, v int, w text);
INSERT INTO qa850 SELECT g, CASE WHEN g % 7 = 3 THEN NULL ELSE g % 10 END, (g * 7) % 13, 'w' || (g % 9) FROM generate_series(1, 400) g;
CREATE UNIQUE INDEX qa850_id ON qa850 (id);
ANALYZE qa850;
CREATE TABLE qb850 (id int, k int, v int, w text);
INSERT INTO qb850 SELECT g, g % 5, (g * 7) % 13, 'w' || (g % 9) FROM generate_series(1, 600) g;
CREATE INDEX qb850_k ON qb850 (k);
CREATE UNIQUE INDEX qb850_id ON qb850 (id);
ANALYZE qb850;
CREATE TABLE qc850 (id int, k int, v int, w text);
INSERT INTO qc850 SELECT g, g % 5, (g * 7) % 13, 'w' || (g % 9) FROM generate_series(1, 150) g;
CREATE INDEX qc850_k ON qc850 (k);
ANALYZE qc850;
CREATE TABLE qd850 (id int, k int, v int, w text);
INSERT INTO qd850 SELECT g, g % 176, (g * 7) % 13, 'w' || (g % 9) FROM generate_series(1, 350) g;
CREATE INDEX qd850_k ON qd850 (k);
ANALYZE qd850;
EXPLAIN SELECT qc850.id, qa850.id, qa850.w FROM qc850 RIGHT JOIN qa850 ON qc850.id = qa850.id AND qc850.id = qa850.v WHERE EXISTS (SELECT 1 FROM qb850 WHERE qb850.id = qa850.v);
SELECT count(*) FROM (SELECT qc850.id, qa850.id, qa850.w FROM qc850 RIGHT JOIN qa850 ON qc850.id = qa850.id AND qc850.id = qa850.v WHERE EXISTS (SELECT 1 FROM qb850 WHERE qb850.id = qa850.v)) s;
EXPLAIN SELECT qa850.id, qc850.id, qc850.w FROM qa850 RIGHT JOIN qc850 ON qa850.id = qc850.k LEFT JOIN qb850 ON qc850.id = qb850.k WHERE qc850.k IS NULL;
SELECT count(*) FROM (SELECT qa850.id, qc850.id, qc850.w FROM qa850 RIGHT JOIN qc850 ON qa850.id = qc850.k LEFT JOIN qb850 ON qc850.id = qb850.k WHERE qc850.k IS NULL) s;
EXPLAIN SELECT qc850.id, qd850.id, qb850.id, qb850.w FROM qc850 LEFT JOIN qd850 ON qc850.k = qd850.k AND qd850.v < 4 FULL JOIN qb850 ON qd850.v = qb850.id;
SELECT count(*) FROM (SELECT qc850.id, qd850.id, qb850.id, qb850.w FROM qc850 LEFT JOIN qd850 ON qc850.k = qd850.k AND qd850.v < 4 FULL JOIN qb850 ON qd850.v = qb850.id) s;
EXPLAIN SELECT qc850.id, qa850.id, qa850.w FROM qc850 JOIN qa850 ON qc850.k = qa850.k LEFT JOIN qd850 ON qa850.v = qd850.k AND qa850.v < qd850.v;
SELECT count(*) FROM (SELECT qc850.id, qa850.id, qa850.w FROM qc850 JOIN qa850 ON qc850.k = qa850.k LEFT JOIN qd850 ON qa850.v = qd850.k AND qa850.v < qd850.v) s;
EXPLAIN SELECT qa850.id, qb850.id, qb850.w FROM qa850 FULL JOIN qb850 ON qa850.v = qb850.k WHERE qa850.v IS NULL AND EXISTS (SELECT 1 FROM qc850 WHERE qc850.k = qa850.v);
SELECT count(*) FROM (SELECT qa850.id, qb850.id, qb850.w FROM qa850 FULL JOIN qb850 ON qa850.v = qb850.k WHERE qa850.v IS NULL AND EXISTS (SELECT 1 FROM qc850 WHERE qc850.k = qa850.v)) s;
EXPLAIN SELECT qb850.id, qa850.id, qd850.id, qd850.w FROM qb850 FULL JOIN qa850 ON qb850.id = qa850.k LEFT JOIN qd850 ON qa850.id = qd850.k AND qa850.v > 6 WHERE qa850.v IN (SELECT qc850.id FROM qc850);
SELECT count(*) FROM (SELECT qb850.id, qa850.id, qd850.id, qd850.w FROM qb850 FULL JOIN qa850 ON qb850.id = qa850.k LEFT JOIN qd850 ON qa850.id = qd850.k AND qa850.v > 6 WHERE qa850.v IN (SELECT qc850.id FROM qc850)) s;
EXPLAIN SELECT qb850.id, qc850.id, qc850.w FROM qb850 RIGHT JOIN qc850 ON qb850.k = qc850.k AND qb850.v = qc850.k LEFT JOIN qd850 ON qb850.k = qd850.k AND qd850.v < 3;
SELECT count(*) FROM (SELECT qb850.id, qc850.id, qc850.w FROM qb850 RIGHT JOIN qc850 ON qb850.k = qc850.k AND qb850.v = qc850.k LEFT JOIN qd850 ON qb850.k = qd850.k AND qd850.v < 3) s;
EXPLAIN SELECT qa850.id, qa850.w FROM qa850 LEFT JOIN qc850 ON qa850.k = qc850.v AND qc850.v < 8 WHERE qc850.v IS NULL AND qc850.k IN (SELECT qb850.v FROM qb850);
SELECT count(*) FROM (SELECT qa850.id, qa850.w FROM qa850 LEFT JOIN qc850 ON qa850.k = qc850.v AND qc850.v < 8 WHERE qc850.v IS NULL AND qc850.k IN (SELECT qb850.v FROM qb850)) s;
EXPLAIN SELECT qb850.id, qb850.w FROM qb850 LEFT JOIN qd850 ON qb850.k = qd850.k WHERE qd850.v < 8;
EXPLAIN SELECT qa850.id, qd850.id, qc850.id, qc850.w FROM qa850 LEFT JOIN qd850 ON qa850.k = qd850.k FULL JOIN qc850 ON qd850.v = qc850.k WHERE qa850.k IS NULL AND EXISTS (SELECT 1 FROM qb850 WHERE qb850.k = qd850.id AND qb850.v < 10);
EXPLAIN SELECT qa850.id, qc850.id, qd850.id, qd850.w FROM qa850 LEFT JOIN qc850 ON qa850.k = qc850.v AND qa850.v > 7 RIGHT JOIN qd850 ON qa850.id = qd850.id WHERE EXISTS (SELECT 1 FROM qb850 WHERE qb850.id = qd850.v);
EXPLAIN SELECT qc850.id, qc850.w FROM qc850 LEFT JOIN qb850 ON qc850.v = qb850.k AND qc850.k = qb850.k WHERE qc850.id IN (SELECT qd850.v FROM qd850 WHERE qd850.v < 12);
EXPLAIN SELECT qc850.id, qc850.w FROM qc850 LEFT JOIN qd850 ON qc850.v = qd850.k;
EXPLAIN SELECT qb850.id, qa850.id, qd850.id, qd850.w FROM qb850 FULL JOIN qa850 ON qb850.k = qa850.id LEFT JOIN qd850 ON qb850.id = qd850.k WHERE qb850.v < 3;
EXPLAIN SELECT qa850.id, qa850.w FROM qa850 LEFT JOIN qd850 ON qa850.k = qd850.k AND qd850.v < 8 WHERE qd850.v < 3 AND EXISTS (SELECT 1 FROM qb850 WHERE qb850.v = qa850.id AND qb850.v < 4);
