-- PostgreSQL 18 pull_up_sublinks: a WHERE clause's EXISTS, NOT EXISTS and
-- IN subqueries become semi and anti joins. The planner hashes the smaller
-- side (Hash Semi/Anti Join, or Hash Right Semi/Anti Join when the query's
-- own relation is smaller) and loops over a materialized copy when no
-- equality joins the two. Hash and materialize memory lines are the
-- allocator's own and are left out.
CREATE TABLE big532 (id int, v int);
CREATE TABLE small532 (id int, aid int, w int);
INSERT INTO big532 SELECT g, g % 7 FROM generate_series(1, 1000) g;
INSERT INTO small532 SELECT g, g * 3, g FROM generate_series(1, 20) g;
INSERT INTO small532 VALUES (99, NULL, 5);
ANALYZE big532;
ANALYZE small532;
CREATE FUNCTION analyzed532(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Buckets|Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
EXPLAIN (COSTS OFF) SELECT * FROM big532 WHERE EXISTS (SELECT 1 FROM small532 WHERE small532.aid = big532.id);
EXPLAIN (COSTS OFF) SELECT * FROM big532 WHERE NOT EXISTS (SELECT 1 FROM small532 WHERE small532.aid = big532.id);
EXPLAIN (COSTS OFF) SELECT * FROM small532 WHERE EXISTS (SELECT 1 FROM big532 WHERE big532.id = small532.aid);
EXPLAIN (COSTS OFF) SELECT * FROM small532 WHERE NOT EXISTS (SELECT 1 FROM big532 WHERE big532.id = small532.aid);
EXPLAIN (COSTS OFF) SELECT * FROM big532 WHERE EXISTS (SELECT 1 FROM small532 WHERE small532.aid = big532.id AND small532.w > big532.v);
EXPLAIN (COSTS OFF) SELECT * FROM big532 WHERE NOT EXISTS (SELECT 1 FROM small532 WHERE small532.w > big532.v);
EXPLAIN (COSTS OFF) SELECT * FROM small532 WHERE EXISTS (SELECT 1 FROM big532 WHERE big532.v > small532.w);
EXPLAIN (COSTS OFF) SELECT * FROM big532 WHERE id IN (SELECT aid FROM small532);
EXPLAIN (COSTS OFF) SELECT * FROM big532 b WHERE v > 3 AND EXISTS (SELECT 1 FROM small532 s WHERE s.aid = b.id AND s.w < 10);
EXPLAIN (COSTS OFF, FORMAT JSON) SELECT id FROM small532 WHERE NOT EXISTS (SELECT 1 FROM big532 WHERE big532.id = small532.aid);
SELECT analyzed532('SELECT * FROM big532 WHERE EXISTS (SELECT 1 FROM small532 WHERE small532.aid = big532.id)');
SELECT analyzed532('SELECT * FROM big532 WHERE NOT EXISTS (SELECT 1 FROM small532 WHERE small532.aid = big532.id)');
SELECT analyzed532('SELECT * FROM small532 WHERE EXISTS (SELECT 1 FROM big532 WHERE big532.id = small532.aid)');
SELECT analyzed532('SELECT * FROM small532 WHERE NOT EXISTS (SELECT 1 FROM big532 WHERE big532.id = small532.aid)');
SELECT analyzed532('SELECT * FROM big532 WHERE NOT EXISTS (SELECT 1 FROM small532 WHERE small532.w > big532.v)');
SELECT analyzed532('SELECT * FROM small532 WHERE EXISTS (SELECT 1 FROM big532 WHERE big532.v > small532.w)');
SELECT analyzed532('SELECT * FROM big532 WHERE v > 3 AND EXISTS (SELECT 1 FROM small532 WHERE small532.aid = big532.id AND small532.w < 10)');
SELECT * FROM big532 WHERE EXISTS (SELECT 1 FROM small532 WHERE small532.aid = big532.id);
SELECT count(*) FROM big532 WHERE NOT EXISTS (SELECT 1 FROM small532 WHERE small532.aid = big532.id);
SELECT * FROM small532 WHERE EXISTS (SELECT 1 FROM big532 WHERE big532.id = small532.aid) ORDER BY id;
SELECT * FROM small532 WHERE NOT EXISTS (SELECT 1 FROM big532 WHERE big532.id = small532.aid) ORDER BY id;
SELECT * FROM big532 WHERE id IN (SELECT aid FROM small532 WHERE w < 10);
-- The subquery's relation answers only its own condition's names.
SELECT aid FROM big532 WHERE EXISTS (SELECT 1 FROM small532 WHERE aid = big532.id);
SELECT small532.w FROM big532 WHERE EXISTS (SELECT 1 FROM small532 WHERE aid = big532.id);
SELECT id FROM big532 WHERE EXISTS (SELECT 1 FROM small532 WHERE aid = big532.id AND w / 0 > 1);
SELECT id FROM big532 WHERE EXISTS (SELECT 1 FROM small532 WHERE small532.nope = big532.id);
CREATE TABLE t532 (id int, x int, y text);
CREATE TABLE u532 (id int, tid int, z text);
CREATE TABLE w532 (k int);
INSERT INTO t532 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, NULL, 'c'), (NULL, 40, 'd'), (5, 50, NULL);
INSERT INTO u532 VALUES (1, 1, 'p'), (2, 1, 'q'), (3, 3, 'r'), (4, NULL, 's'), (5, 9, NULL);
INSERT INTO w532 VALUES (1), (3), (NULL);
SELECT * FROM t532 WHERE NOT EXISTS (SELECT 1 FROM u532 WHERE u532.tid = t532.id) ORDER BY id;
SELECT * FROM t532 WHERE id IN (SELECT k FROM w532) AND EXISTS (SELECT 1 FROM u532 WHERE tid = t532.id AND z > 'p') ORDER BY id;
SELECT t.id FROM t532 t LEFT JOIN u532 u ON t.id = u.id WHERE NOT EXISTS (SELECT 1 FROM w532 WHERE w532.k = u.tid) ORDER BY 1;
SELECT id FROM t532 WHERE EXISTS (SELECT 1 FROM u532 WHERE u532.tid = t532.id OR u532.z = t532.y) ORDER BY id;
SELECT (SELECT count(*) FROM t532 WHERE EXISTS (SELECT 1 FROM u532 WHERE u532.tid = t532.id AND u532.id < w532.k)) FROM w532 ORDER BY 1;
DROP FUNCTION analyzed532(text);
DROP TABLE big532, small532, t532, u532, w532;
