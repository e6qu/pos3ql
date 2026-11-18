-- A parenthesized join inside another join's side (a LEFT JOIN (b JOIN c
-- ON x) ON y) is a node of the join tree PostgreSQL's deconstruct_jointree
-- reads: each of its relations joins in the same search, the outer join's
-- order restrictions applying, and each joined row fills the parenthesized
-- join's columns from its members' rows.
CREATE TABLE na934 (id int, k int, v text);
CREATE TABLE nb934 (id int, k int, w int);
CREATE TABLE nc934 (id int, k int, z int);
CREATE TABLE nd934 (id int, q int);
INSERT INTO na934 SELECT g, g % 40, 'a' || g FROM generate_series(1, 400) g;
INSERT INTO nb934 SELECT g, g % 30, g % 7 FROM generate_series(1, 300) g;
INSERT INTO nc934 SELECT g, g % 20, g % 5 FROM generate_series(1, 200) g;
INSERT INTO nd934 SELECT g, g % 9 FROM generate_series(1, 100) g;
CREATE INDEX nc934_k ON nc934 (k);
ANALYZE na934, nb934, nc934, nd934;
EXPLAIN SELECT * FROM na934 LEFT JOIN (nb934 JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id;
EXPLAIN SELECT * FROM na934 FULL JOIN (nb934 JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id;
EXPLAIN SELECT * FROM na934 JOIN (nb934 LEFT JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id;
EXPLAIN SELECT * FROM na934 LEFT JOIN (nb934 JOIN (nc934 JOIN nd934 ON nc934.id = nd934.id) ON nb934.id = nc934.id) ON na934.id = nb934.id;
EXPLAIN (COSTS OFF) SELECT * FROM na934 LEFT JOIN (nb934 JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id ORDER BY na934.id LIMIT 4;
SELECT * FROM na934 LEFT JOIN (nb934 JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id ORDER BY na934.id DESC LIMIT 4;
SELECT count(*), count(nc934.id), sum(nb934.w) FROM na934 LEFT JOIN (nb934 LEFT JOIN nc934 ON nb934.k = nc934.k) ON na934.k = nb934.k WHERE na934.id < 50;
SELECT na934.id, nb934.id, nc934.id FROM na934 FULL JOIN (nb934 JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id + 350 ORDER BY 1, 2 LIMIT 6;
EXPLAIN (VERBOSE, COSTS OFF) SELECT na934.v, nc934.z FROM na934 LEFT JOIN (nb934 JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id;
SELECT na934.v, nc934.z FROM na934 LEFT JOIN (nb934 JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id WHERE na934.id > 395;
SELECT na934.id, (nb934, nc934) IS NULL AS unmatched FROM na934 LEFT JOIN (nb934 JOIN nc934 ON nb934.id = nc934.id) ON na934.id = nb934.id WHERE na934.id BETWEEN 198 AND 202 ORDER BY 1;
DROP TABLE na934, nb934, nc934, nd934;
