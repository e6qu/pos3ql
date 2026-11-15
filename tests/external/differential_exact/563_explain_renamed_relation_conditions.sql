-- PostgreSQL 18 names each range table entry uniquely: a WITH query's
-- relations named like the statement's take a suffix (r_1, e1_1), and the
-- conditions and target lists of their query block print the new names. A
-- recursive term joins the working table with a hash join.
CREATE TABLE e563 (id int, parent int);
INSERT INTO e563 SELECT g, g / 2 FROM generate_series(1, 40) g;
ANALYZE e563;
EXPLAIN (COSTS OFF) WITH RECURSIVE r(id) AS (SELECT 1 UNION ALL SELECT e563.id FROM e563 JOIN r ON e563.parent = r.id) SELECT * FROM r;
EXPLAIN (COSTS OFF) WITH RECURSIVE r(id) AS (SELECT 1 UNION ALL SELECT e563.id FROM r, e563 WHERE e563.parent = r.id) SELECT * FROM r;
EXPLAIN (COSTS OFF) WITH RECURSIVE r(id, d) AS (SELECT 1, 0 UNION ALL SELECT e563.id, r.d + 1 FROM e563 JOIN r ON e563.parent = r.id WHERE r.d < 3) SELECT * FROM r;
EXPLAIN (VERBOSE, COSTS OFF) WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r;
EXPLAIN (COSTS OFF) WITH RECURSIVE r(id) AS (SELECT id FROM e563 WHERE id = 1 UNION ALL SELECT e563.id FROM e563 JOIN r ON e563.parent = r.id) SELECT * FROM r;
WITH RECURSIVE r(id, d) AS (SELECT 1, 0 UNION ALL SELECT e563.id, r.d + 1 FROM e563 JOIN r ON e563.parent = r.id WHERE r.d < 3) SELECT * FROM r ORDER BY id;
EXPLAIN (COSTS OFF) SELECT * FROM e563 WHERE id = (SELECT max(id) FROM e563 WHERE parent > 3);
