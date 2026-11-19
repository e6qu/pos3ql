-- A merge join reads an index-only scan in order as far as it fetches:
-- the scan counts the rows returned and the heap fetches of those on pages
-- the visibility map does not mark all-visible.
CREATE TABLE mj945 (id int PRIMARY KEY, v int);
CREATE TABLE mk945 (id int PRIMARY KEY, w int);
INSERT INTO mj945 SELECT i, i FROM generate_series(1, 3000) i;
INSERT INTO mk945 SELECT i * 2, i FROM generate_series(1, 1000) i;
ANALYZE mj945, mk945;
SET enable_hashjoin = off;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF) SELECT mj945.id FROM mj945 JOIN mk945 ON mj945.id = mk945.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT mj945.id FROM mj945 JOIN mk945 ON mj945.id = mk945.id;
VACUUM mj945, mk945;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT mj945.id FROM mj945 JOIN mk945 ON mj945.id = mk945.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT mj945.id FROM mj945 JOIN mk945 ON mj945.id = mk945.id WHERE mk945.id < 1000;
RESET enable_hashjoin;
RESET enable_nestloop;
DROP TABLE mj945, mk945;
