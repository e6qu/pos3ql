-- Join conditions comparing two types of one operator family (int4 =
-- int8, int2 = int4, real = double precision): one equivalence class
-- holds both columns, so merge and hash joins and the class's implied
-- equalities use them as PostgreSQL's planner does.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/equivclass.c
CREATE TABLE xa882 (id int, k int4, s int2, f real);
CREATE TABLE xb882 (id int, k int8, s int4, f float8);
CREATE TABLE xc882 (id int, k int2);
INSERT INTO xa882 SELECT g, g % 60, (g % 25)::int2, (g % 16) * 0.25 FROM generate_series(1, 500) g;
INSERT INTO xb882 SELECT g, g % 80, g % 30, (g % 20) * 0.25 FROM generate_series(1, 400) g;
INSERT INTO xc882 SELECT g, (g % 40)::int2 FROM generate_series(1, 200) g;
CREATE INDEX xb882_k ON xb882 (k);
ANALYZE xa882;
ANALYZE xb882;
ANALYZE xc882;
EXPLAIN SELECT xa882.id, xb882.id FROM xa882 JOIN xb882 ON xa882.k = xb882.k WHERE xa882.id < 50;
SELECT xa882.id, xb882.id FROM xa882 JOIN xb882 ON xa882.k = xb882.k WHERE xa882.id < 50;
EXPLAIN SELECT xa882.id, xb882.id, xc882.id FROM xa882 JOIN xb882 ON xa882.k = xb882.k JOIN xc882 ON xc882.k = xb882.k WHERE xc882.id < 10;
SELECT xa882.id, xb882.id, xc882.id FROM xa882 JOIN xb882 ON xa882.k = xb882.k JOIN xc882 ON xc882.k = xb882.k WHERE xc882.id < 10;
EXPLAIN SELECT xa882.id, xb882.id FROM xa882 JOIN xb882 ON xa882.s = xb882.s AND xa882.f = xb882.f;
SELECT count(*), sum(xa882.id), sum(xb882.id) FROM xa882 JOIN xb882 ON xa882.s = xb882.s AND xa882.f = xb882.f;
EXPLAIN SELECT xa882.id, xb882.id FROM xa882 LEFT JOIN xb882 ON xa882.k = xb882.k AND xb882.id < 5 ORDER BY xa882.k, xa882.id LIMIT 12;
SELECT xa882.id, xb882.id FROM xa882 LEFT JOIN xb882 ON xa882.k = xb882.k AND xb882.id < 5 ORDER BY xa882.k, xa882.id LIMIT 12;
EXPLAIN SELECT xa882.id, xb882.id FROM xa882 JOIN xb882 ON xa882.k = xb882.k WHERE xa882.k = 7;
SELECT xa882.id, xb882.id FROM xa882 JOIN xb882 ON xa882.k = xb882.k WHERE xa882.k = 7;
EXPLAIN SELECT xa882.id FROM xa882 WHERE EXISTS (SELECT 1 FROM xb882 WHERE xb882.k = xa882.k AND xb882.id < 30);
SELECT xa882.id FROM xa882 WHERE EXISTS (SELECT 1 FROM xb882 WHERE xb882.k = xa882.k AND xb882.id < 30);
