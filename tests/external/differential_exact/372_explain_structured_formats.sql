-- EXPLAIN's JSON, XML and YAML documents follow PostgreSQL's layout: one
-- property per line, nested plans with their parent relationship, join
-- types, aliases beside relation names, and settings and summary groups.
CREATE TABLE a372 (x int, y int);
CREATE TABLE b372 (x int, z int);
SET search_path = public;
EXPLAIN (COSTS OFF, FORMAT JSON) SELECT * FROM a372 q WHERE q.x = 1;
EXPLAIN (COSTS OFF, FORMAT YAML) SELECT * FROM a372, b372 q WHERE a372.y < q.z;
EXPLAIN (COSTS OFF, FORMAT XML) SELECT * FROM a372 JOIN b372 ON a372.y < b372.z
  WHERE b372.x = 2;
EXPLAIN (COSTS OFF, FORMAT JSON) UPDATE a372 SET y = 1 WHERE x = 3;
EXPLAIN (COSTS OFF, FORMAT YAML) INSERT INTO a372 VALUES (1, 2), (3, 4);
EXPLAIN (SETTINGS, COSTS OFF, FORMAT JSON) SELECT * FROM a372;
EXPLAIN (SETTINGS, COSTS OFF, FORMAT XML) SELECT * FROM a372;
EXPLAIN (SETTINGS, COSTS OFF, FORMAT YAML) SELECT * FROM a372;
EXPLAIN (COSTS OFF) SELECT * FROM a372 q, b372 r WHERE q.y < r.z;
