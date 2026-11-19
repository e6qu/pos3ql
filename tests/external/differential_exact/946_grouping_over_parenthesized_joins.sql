-- Grouping, aggregates and DISTINCT over a parenthesized join are planned
-- by the join search over its members, nested to any depth; a hashed
-- grouping over a join reports its memory.
CREATE TABLE pa (id int PRIMARY KEY, x int);
CREATE TABLE pb (id int PRIMARY KEY, y int);
CREATE TABLE pc (id int PRIMARY KEY, z int);
CREATE TABLE pd (id int PRIMARY KEY, w int);
INSERT INTO pa SELECT i, i % 10 FROM generate_series(1, 300) i;
INSERT INTO pb SELECT i, i % 7 FROM generate_series(1, 250) i;
INSERT INTO pc SELECT i, i % 5 FROM generate_series(1, 200) i;
INSERT INTO pd SELECT i, i % 3 FROM generate_series(1, 300) i;
ANALYZE pa, pb, pc, pd;
SELECT count(*) FROM pa LEFT JOIN (pb JOIN (pc JOIN pd ON pc.id = pd.id) ON pb.id = pc.id) ON pa.id = pb.id;
EXPLAIN (COSTS OFF) SELECT count(*) FROM pa LEFT JOIN (pb JOIN (pc JOIN pd ON pc.id = pd.id) ON pb.id = pc.id) ON pa.id = pb.id;
EXPLAIN SELECT count(*) FROM pa LEFT JOIN (pb JOIN (pc JOIN pd ON pc.id = pd.id) ON pb.id = pc.id) ON pa.id = pb.id;
SELECT pa.x, count(*), sum(pc.z) FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id GROUP BY pa.x ORDER BY 1 LIMIT 4;
EXPLAIN SELECT pa.x, count(*) FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id GROUP BY pa.x;
SELECT pc.z, count(*), max(pa.x) FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id GROUP BY pc.z ORDER BY 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT pc.z, count(*) FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id GROUP BY pc.z;
SET enable_hashagg = off;
EXPLAIN SELECT pc.z, count(*) FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id GROUP BY pc.z;
SELECT pc.z, count(*), string_agg(pa.id::text, ',' ORDER BY pa.id) FILTER (WHERE pa.id < 30) FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id GROUP BY pc.z ORDER BY 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT pc.z, count(*) FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id GROUP BY pc.z LIMIT 2;
SELECT DISTINCT pc.z FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT DISTINCT pc.z FROM pa LEFT JOIN (pb JOIN pc ON pb.id = pc.id) ON pa.id = pb.id;
RESET enable_hashagg;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT pb946.y, count(*) FROM pa946 LEFT JOIN pb946 ON pa946.id = pb946.id GROUP BY pb946.y;
DROP TABLE pa946, pb946, pc946, pd946;
