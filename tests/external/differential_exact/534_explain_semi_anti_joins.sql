-- PostgreSQL 18 pull_up_sublinks: a WHERE clause's EXISTS, NOT EXISTS and
-- IN subqueries become semi and anti joins. The planner hashes the smaller
-- side (Hash Semi/Anti Join, or Hash Right Semi/Anti Join when the query's
-- own relation is smaller) and loops over a materialized copy when no
-- equality joins the two. Hash and materialize memory lines are the
-- allocator's own and are left out.
CREATE TABLE big534 (id int, v int);
CREATE TABLE small534 (id int, aid int, w int);
INSERT INTO big534 SELECT g, g % 7 FROM generate_series(1, 1000) g;
INSERT INTO small534 SELECT g, g * 3, g FROM generate_series(1, 20) g;
INSERT INTO small534 VALUES (99, NULL, 5);
ANALYZE big534;
ANALYZE small534;
CREATE FUNCTION analyzed534(query text) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE
  line text;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
    CONTINUE WHEN line ~ '^\s*(Batches|Storage):';
    RETURN NEXT line;
  END LOOP;
END
$$;
EXPLAIN (COSTS OFF) SELECT * FROM big534 WHERE EXISTS (SELECT 1 FROM small534 WHERE small534.aid = big534.id);
EXPLAIN (COSTS OFF) SELECT * FROM big534 WHERE NOT EXISTS (SELECT 1 FROM small534 WHERE small534.aid = big534.id);
EXPLAIN (COSTS OFF) SELECT * FROM small534 WHERE EXISTS (SELECT 1 FROM big534 WHERE big534.id = small534.aid);
EXPLAIN (COSTS OFF) SELECT * FROM small534 WHERE NOT EXISTS (SELECT 1 FROM big534 WHERE big534.id = small534.aid);
EXPLAIN (COSTS OFF) SELECT * FROM big534 WHERE EXISTS (SELECT 1 FROM small534 WHERE small534.aid = big534.id AND small534.w > big534.v);
EXPLAIN (COSTS OFF) SELECT * FROM big534 WHERE NOT EXISTS (SELECT 1 FROM small534 WHERE small534.w > big534.v);
EXPLAIN (COSTS OFF) SELECT * FROM small534 WHERE EXISTS (SELECT 1 FROM big534 WHERE big534.v > small534.w);
EXPLAIN (COSTS OFF) SELECT * FROM big534 WHERE id IN (SELECT aid FROM small534);
EXPLAIN (COSTS OFF) SELECT * FROM big534 b WHERE v > 3 AND EXISTS (SELECT 1 FROM small534 s WHERE s.aid = b.id AND s.w < 10);
EXPLAIN (COSTS OFF, FORMAT JSON) SELECT id FROM small534 WHERE NOT EXISTS (SELECT 1 FROM big534 WHERE big534.id = small534.aid);
SELECT analyzed534('SELECT * FROM big534 WHERE EXISTS (SELECT 1 FROM small534 WHERE small534.aid = big534.id)');
SELECT analyzed534('SELECT * FROM big534 WHERE NOT EXISTS (SELECT 1 FROM small534 WHERE small534.aid = big534.id)');
SELECT analyzed534('SELECT * FROM small534 WHERE EXISTS (SELECT 1 FROM big534 WHERE big534.id = small534.aid)');
SELECT analyzed534('SELECT * FROM small534 WHERE NOT EXISTS (SELECT 1 FROM big534 WHERE big534.id = small534.aid)');
SELECT analyzed534('SELECT * FROM big534 WHERE NOT EXISTS (SELECT 1 FROM small534 WHERE small534.w > big534.v)');
SELECT analyzed534('SELECT * FROM small534 WHERE EXISTS (SELECT 1 FROM big534 WHERE big534.v > small534.w)');
SELECT analyzed534('SELECT * FROM big534 WHERE v > 3 AND EXISTS (SELECT 1 FROM small534 WHERE small534.aid = big534.id AND small534.w < 10)');
SELECT * FROM big534 WHERE EXISTS (SELECT 1 FROM small534 WHERE small534.aid = big534.id);
SELECT count(*) FROM big534 WHERE NOT EXISTS (SELECT 1 FROM small534 WHERE small534.aid = big534.id);
SELECT * FROM small534 WHERE EXISTS (SELECT 1 FROM big534 WHERE big534.id = small534.aid) ORDER BY id;
SELECT * FROM small534 WHERE NOT EXISTS (SELECT 1 FROM big534 WHERE big534.id = small534.aid) ORDER BY id;
SELECT * FROM big534 WHERE id IN (SELECT aid FROM small534 WHERE w < 10);
-- The subquery's relation answers only its own condition's names.
SELECT aid FROM big534 WHERE EXISTS (SELECT 1 FROM small534 WHERE aid = big534.id);
SELECT small534.w FROM big534 WHERE EXISTS (SELECT 1 FROM small534 WHERE aid = big534.id);
SELECT id FROM big534 WHERE EXISTS (SELECT 1 FROM small534 WHERE aid = big534.id AND w / 0 > 1);
SELECT id FROM big534 WHERE EXISTS (SELECT 1 FROM small534 WHERE small534.nope = big534.id);
CREATE TABLE t534 (id int, x int, y text);
CREATE TABLE u534 (id int, tid int, z text);
CREATE TABLE w534 (k int);
INSERT INTO t534 VALUES (1, 10, 'a'), (2, 20, 'b'), (3, NULL, 'c'), (NULL, 40, 'd'), (5, 50, NULL);
INSERT INTO u534 VALUES (1, 1, 'p'), (2, 1, 'q'), (3, 3, 'r'), (4, NULL, 's'), (5, 9, NULL);
INSERT INTO w534 VALUES (1), (3), (NULL);
SELECT * FROM t534 WHERE NOT EXISTS (SELECT 1 FROM u534 WHERE u534.tid = t534.id) ORDER BY id;
SELECT * FROM t534 WHERE id IN (SELECT k FROM w534) AND EXISTS (SELECT 1 FROM u534 WHERE tid = t534.id AND z > 'p') ORDER BY id;
SELECT t.id FROM t534 t LEFT JOIN u534 u ON t.id = u.id WHERE NOT EXISTS (SELECT 1 FROM w534 WHERE w534.k = u.tid) ORDER BY 1;
SELECT id FROM t534 WHERE EXISTS (SELECT 1 FROM u534 WHERE u534.tid = t534.id OR u534.z = t534.y) ORDER BY id;
SELECT (SELECT count(*) FROM t534 WHERE EXISTS (SELECT 1 FROM u534 WHERE u534.tid = t534.id AND u534.id < w534.k)) FROM w534 ORDER BY 1;
DROP FUNCTION analyzed534(text);
DROP TABLE big534, small534, t534, u534, w534;
