-- UPDATE ... FROM and DELETE ... USING into a partitioned table or an
-- inheritance tree plan one join over the target's Append, the write
-- listing each member pruning keeps; a hash join builds on the FROM table
-- and keeps the last FROM row a target row joins, as PostgreSQL's chains do.
CREATE TABLE p630 (id int, v int, x text) PARTITION BY RANGE (id);
CREATE TABLE p630_1 PARTITION OF p630 FOR VALUES FROM (1) TO (500);
CREATE TABLE p630_2 PARTITION OF p630 FOR VALUES FROM (500) TO (1001);
INSERT INTO p630 SELECT g, g, 'x' FROM generate_series(1, 1000) g;
CREATE TABLE s630 (k int, w int, y text);
INSERT INTO s630 SELECT (g % 20) * 50 + 1, g, 'y' || g FROM generate_series(1, 40) g;
CREATE TABLE ip630 (a int, v int);
CREATE TABLE ip630_1 (CHECK (a < 10)) INHERITS (ip630);
INSERT INTO ip630 SELECT g, g FROM generate_series(10, 500) g;
INSERT INTO ip630_1 SELECT g, g FROM generate_series(1, 9) g;
ANALYZE p630;
ANALYZE s630;
ANALYZE ip630;
ANALYZE ip630_1;
EXPLAIN (COSTS OFF) UPDATE p630 SET v = s630.w FROM s630 WHERE p630.id = s630.k;
EXPLAIN (COSTS OFF) UPDATE p630 SET v = s630.w FROM s630 WHERE p630.id = s630.k AND p630.id < 100;
EXPLAIN (COSTS OFF) UPDATE p630 SET v = s630.w FROM s630 WHERE p630.id = s630.k AND p630.id > 5000;
EXPLAIN (COSTS OFF) UPDATE p630 SET v = s630.w FROM s630
  WHERE p630.id = s630.k AND p630.v + s630.w > 3 AND s630.w > 2;
EXPLAIN (COSTS OFF) DELETE FROM p630 USING s630 WHERE p630.id = s630.k AND p630.v > 900;
EXPLAIN (COSTS OFF) DELETE FROM p630 USING s630 WHERE p630.v < s630.k;
EXPLAIN (COSTS OFF) UPDATE ip630 SET v = s630.w FROM s630 WHERE ip630.a = s630.k;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE p630 SET x = s630.y || 'a', v = p630.v + s630.w
  FROM s630 WHERE p630.id = s630.k;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE p630 SET v = 1 FROM s630
  WHERE s630.w + p630.v > 3 AND p630.id = s630.k RETURNING s630.y, p630.x, s630.*;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM p630 USING s630 WHERE p630.id = s630.k RETURNING *;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM p630 USING s630 WHERE p630.v < s630.k;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE p630 SET v = s630.w FROM s630
  WHERE p630.id = s630.k AND p630.id < 100 RETURNING p630.v;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE ip630 SET v = s630.w FROM s630 WHERE ip630.a = s630.k;
UPDATE p630 SET v = s630.w FROM s630 WHERE p630.id = s630.k RETURNING p630.*, s630.k, s630.w;
UPDATE p630 SET id = id + 400, v = s630.w FROM s630
  WHERE p630.id = s630.k + 2 AND p630.id < 300 RETURNING tableoid::regclass, p630.*, s630.w;
DELETE FROM p630 USING s630 WHERE p630.id = s630.k AND p630.id > 900 RETURNING p630.id, s630.w;
SELECT tableoid::regclass, * FROM p630 WHERE v <> id ORDER BY id;
UPDATE ip630 SET v = s630.w FROM s630 WHERE ip630.a = s630.k - 50 RETURNING tableoid::regclass, ip630.*, s630.w;
SELECT tableoid::regclass, * FROM ip630 WHERE v <> a ORDER BY a;
