-- A full join's merged USING column is COALESCE of its sides (PostgreSQL's
-- join alias variable). Grouped or made distinct, it is an expression key
-- whose class the join of its relations computes; a sorted grouping reads
-- it from its input, parenthesized. Equated with a constant, it joins that
-- constant's equivalence class: reconsider_full_join_clause then equates
-- each side of the full join with the constant, through every lower full
-- join's own COALESCE, and the join keeps no condition; an outer join's
-- clause of it moves the constant to the inner side. USING and NATURAL
-- joins beside a parenthesized join join on its members' columns.
CREATE TABLE fu (id int, v int);
INSERT INTO fu SELECT g, g % 7 FROM generate_series(1, 200) g;
ANALYZE fu;
CREATE TABLE pa (id int, v int);
INSERT INTO pa SELECT g, g % 3 FROM generate_series(1, 1000) g;
ANALYZE pa;
CREATE TABLE pb (id int, w int);
INSERT INTO pb SELECT g, g % 5 FROM generate_series(1, 50) g;
ANALYZE pb;
CREATE TABLE pc (id int, x int);
INSERT INTO pc SELECT g % 100, g FROM generate_series(1, 300) g;
ANALYZE pc;

EXPLAIN (COSTS OFF) SELECT id, count(*) FROM fu t0 FULL JOIN fu t1 USING (id) GROUP BY id;
EXPLAIN (COSTS OFF)
  SELECT id, count(*) FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id) GROUP BY id;
SELECT id, count(*) FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id)
  GROUP BY id ORDER BY 2 DESC, 1 LIMIT 3;
EXPLAIN (VERBOSE, COSTS OFF)
  SELECT id, sum(t2.v) FROM fu t0 FULL JOIN fu t1 USING (id) JOIN fu t2 USING (id)
  GROUP BY id HAVING sum(t2.v) > 2;
EXPLAIN (COSTS OFF) SELECT DISTINCT id FROM fu t0 FULL JOIN fu t1 USING (id);
SELECT DISTINCT id FROM fu t0 FULL JOIN fu t1 USING (id) WHERE t1.v = 3 ORDER BY id LIMIT 4;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF)
  SELECT id, count(*) FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id)
  GROUP BY id ORDER BY id DESC;
SELECT id, count(*) FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id)
  GROUP BY id ORDER BY id DESC LIMIT 3;
SELECT id, t0.v, count(*) FROM fu t0 FULL JOIN fu t1 USING (id) GROUP BY id, t0.v ORDER BY 3, 1 LIMIT 3;
RESET enable_hashagg;

EXPLAIN (COSTS OFF) SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) WHERE id = 5;
EXPLAIN (COSTS OFF) SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id) WHERE id = 5;
SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id) WHERE id = 5;
EXPLAIN (COSTS OFF) SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id) WHERE 5 = id;
EXPLAIN (COSTS OFF)
  SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id) FULL JOIN fu t3 USING (id)
  WHERE id = 7;
SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id) FULL JOIN fu t3 USING (id)
  WHERE id = 7;
EXPLAIN (COSTS OFF) SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) LEFT JOIN fu t2 USING (id) WHERE id = 9;
SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) LEFT JOIN fu t2 USING (id) WHERE id = 9;
EXPLAIN (COSTS OFF) SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) WHERE id = 300;
SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) WHERE id = 300;

EXPLAIN (COSTS OFF) SELECT * FROM pa JOIN (pb JOIN pc USING (id)) USING (id);
SELECT * FROM pa JOIN (pb JOIN pc USING (id)) USING (id) LIMIT 5;
SELECT count(*), sum(x) FROM pa JOIN (pb JOIN pc USING (id)) USING (id);
EXPLAIN (COSTS OFF) SELECT * FROM pa LEFT JOIN (pb LEFT JOIN pc USING (id)) USING (id);
SELECT * FROM pa LEFT JOIN (pb LEFT JOIN pc USING (id)) USING (id) ORDER BY 1 LIMIT 4;
EXPLAIN (COSTS OFF) SELECT * FROM (pa FULL JOIN pb USING (id)) JOIN pc USING (id);
SELECT * FROM (pa FULL JOIN pb USING (id)) JOIN pc USING (id) LIMIT 5;
EXPLAIN (COSTS OFF) SELECT * FROM pc JOIN (pa NATURAL JOIN pb) USING (id);
SELECT * FROM pc NATURAL JOIN (pa JOIN pb ON pa.id = pb.id) LIMIT 3;
DROP TABLE fu, pa, pb, pc;
