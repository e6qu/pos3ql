-- An aliased parenthesized join, one of USING or NATURAL joins and one
-- under window functions join in the query's join search, as PostgreSQL's
-- planner reads a join's alias variables: a column of the join is its
-- member's column, a merged column the left member's (inner and LEFT
-- joins), the right one's (RIGHT) or their COALESCE (FULL). A member named
-- as an earlier FROM item is printed with PostgreSQL's suffix.
CREATE TABLE na (id int, k int, v text);
CREATE TABLE nb (id int, k int, w int);
CREATE TABLE nc (id int, k int, z int);
CREATE TABLE nd (id bigint, q int);
INSERT INTO na SELECT g, g % 40, 'a' || g FROM generate_series(1, 400) g;
INSERT INTO nb SELECT g, g % 30, g % 7 FROM generate_series(1, 300) g;
INSERT INTO nc SELECT g, g % 20, g % 5 FROM generate_series(1, 200) g;
INSERT INTO nd SELECT g, g % 9 FROM generate_series(1, 100) g;
ANALYZE na, nb, nc, nd;
EXPLAIN (COSTS OFF) SELECT * FROM na LEFT JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON na.id = j.w;
EXPLAIN SELECT na.id, j.z FROM na LEFT JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON na.id = j.w;
SELECT * FROM na LEFT JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON na.id = j.w ORDER BY na.id, j.z LIMIT 5;
SELECT j.* FROM na JOIN (nb JOIN nc ON nb.id = nc.id) AS j(a, b, c) ON na.id = j.a WHERE j.c = 3 ORDER BY 1 LIMIT 4;
EXPLAIN (VERBOSE, COSTS OFF) SELECT j.* FROM na JOIN (nb JOIN nc ON nb.id = nc.id) AS j(a, b, c) ON na.id = j.a WHERE j.c = 3;
EXPLAIN SELECT * FROM na LEFT JOIN (nb JOIN nc USING (id)) ON na.id = nb.id;
SELECT * FROM na LEFT JOIN (nb JOIN nc USING (id)) ON na.id = nb.id WHERE na.id > 196 ORDER BY na.id LIMIT 6;
EXPLAIN SELECT * FROM na JOIN (nb FULL JOIN nc USING (id)) ON na.id = id;
SELECT id, na.v FROM na JOIN (nb FULL JOIN nc USING (id)) ON na.id = id WHERE id BETWEEN 198 AND 203 ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, na.v FROM na JOIN (nb FULL JOIN nc USING (id)) ON na.id = id WHERE id > 150;
EXPLAIN (COSTS OFF) SELECT * FROM na JOIN (nb RIGHT JOIN nc USING (id)) j ON na.id = j.id;
SELECT count(*), sum(j.id) FROM na JOIN (nb RIGHT JOIN nc USING (id)) j ON na.id = j.id;
EXPLAIN SELECT * FROM na LEFT JOIN (nb NATURAL JOIN nc) ON na.k = nb.k;
SELECT count(*), count(nc.z) FROM na LEFT JOIN (nb NATURAL JOIN nc) ON na.k = nb.k;
EXPLAIN SELECT j.k, count(*) FROM na JOIN (nb JOIN nc USING (id)) j ON na.id = j.id GROUP BY j.k;
SELECT j.k, count(*) FROM na JOIN (nb JOIN nc USING (id)) j ON na.id = j.id GROUP BY j.k ORDER BY 1 LIMIT 3;
SELECT j.w, count(*) FROM na JOIN (nb JOIN nc USING (id)) j ON na.id = j.id GROUP BY j.z;
SELECT nb.id FROM na JOIN (nb JOIN nc USING (id)) j ON na.id = j.id;
SELECT * FROM na JOIN (nb JOIN nd USING (id)) ON na.id = nb.id ORDER BY na.id LIMIT 2;
EXPLAIN (COSTS OFF) SELECT * FROM nb JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON nb.id = j.w;
SELECT nb.id, j.z FROM nb JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON nb.id = j.w ORDER BY 1, 2 LIMIT 3;
EXPLAIN (COSTS OFF) SELECT na.id FROM na JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON na.id = j.w WHERE EXISTS (SELECT 1 FROM nd WHERE nd.q = j.z);
SELECT count(*) FROM na JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON na.id = j.w WHERE EXISTS (SELECT 1 FROM nd WHERE nd.q = j.z);
EXPLAIN (COSTS OFF) SELECT na.id, row_number() OVER (ORDER BY nc.z, na.id) FROM na LEFT JOIN (nb JOIN nc ON nb.id = nc.id) ON na.id = nb.id;
EXPLAIN SELECT na.id, row_number() OVER (ORDER BY nc.z, na.id) FROM na LEFT JOIN (nb JOIN nc ON nb.id = nc.id) ON na.id = nb.id;
SELECT na.id, nc.z, nb.w, row_number() OVER (ORDER BY nc.z, na.id) FROM na LEFT JOIN (nb JOIN nc ON nb.id = nc.id) ON na.id = nb.id ORDER BY na.id LIMIT 5;
EXPLAIN SELECT na.id, j.z, rank() OVER (PARTITION BY j.z ORDER BY na.id) FROM na LEFT JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON na.id = j.w;
SELECT na.id, j.z, rank() OVER (PARTITION BY j.z ORDER BY na.id) FROM na LEFT JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON na.id = j.w ORDER BY 3 DESC, 1 LIMIT 5;
SELECT na.id, sum(nc.z) OVER (PARTITION BY nb.w ORDER BY na.id) FROM na LEFT JOIN (nb JOIN nc USING (id)) ON na.id = nb.id ORDER BY 1 DESC LIMIT 4;
SELECT j, j::text FROM na JOIN (nb JOIN nc ON nb.id = nc.id) AS j ON na.id = j.w WHERE j.z = 0 ORDER BY 2 LIMIT 3;
SELECT row_to_json(j), j.z FROM (nb JOIN nc USING (id)) AS j WHERE j.id = 7;
DROP TABLE na, nb, nc, nd;
