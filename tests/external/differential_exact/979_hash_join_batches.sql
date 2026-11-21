-- A hash join whose inner side PostgreSQL splits into batches under a small
-- work_mem: the outer rows of later batches are saved and probed when their
-- batch loads, with the batch counts EXPLAIN ANALYZE reports.
CREATE TABLE hb1 (id int, v int);
INSERT INTO hb1 SELECT g, g % 7 FROM generate_series(1, 5000) g;
CREATE TABLE hb2 (id int, w int);
INSERT INTO hb2 SELECT g, g % 11 FROM generate_series(1, 5000) g;
ANALYZE hb1, hb2;
SET work_mem = '64kB';
EXPLAIN (COSTS OFF) SELECT count(*) FROM hb1 JOIN hb2 USING (id);
SELECT count(*) FROM hb1 JOIN hb2 USING (id);
SELECT count(*), sum(v), sum(w) FROM hb1 JOIN hb2 ON hb2.id = hb1.id + 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT count(*) FROM hb1 JOIN hb2 USING (id);
EXPLAIN (COSTS OFF) SELECT count(*) FROM hb1 LEFT JOIN hb2 ON hb2.id = hb1.id * 2;
SELECT count(*), count(hb2.id) FROM hb1 LEFT JOIN hb2 ON hb2.id = hb1.id * 2;
EXPLAIN (COSTS OFF) SELECT count(*) FROM hb1 FULL JOIN hb2 ON hb2.id = hb1.id * 2;
SELECT count(*), count(hb1.id), count(hb2.id) FROM hb1 FULL JOIN hb2 ON hb2.id = hb1.id * 2;
SELECT hb1.id, v, w FROM hb1 JOIN hb2 USING (id) WHERE hb1.id % 1000 = 0 ORDER BY 1;
RESET work_mem;
