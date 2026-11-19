-- A scan's filter testing a subplan costs what cost_subplan charges: a hash
-- table loaded once, or the plan's output rescanned per row, plus one
-- operator per compared column, and keeps half the rows (boolvarsel).
CREATE TABLE sa (id int PRIMARY KEY, k int, v text);
CREATE TABLE sb (id int PRIMARY KEY, k int);
INSERT INTO sa SELECT g, g % 50, 'v' || g FROM generate_series(1, 1000) g;
INSERT INTO sb SELECT g, g % 30 FROM generate_series(1, 600) g;
ANALYZE sa, sb;
EXPLAIN SELECT * FROM sa WHERE id NOT IN (SELECT id FROM sb WHERE k = 3);
EXPLAIN SELECT * FROM sa WHERE id NOT IN (SELECT id FROM sb WHERE k = 3) AND k < 10;
EXPLAIN SELECT * FROM sa WHERE k = 4 OR id NOT IN (SELECT id FROM sb WHERE k = 3);
EXPLAIN SELECT * FROM sa WHERE id = ANY (SELECT id FROM sb WHERE k = 3) IS NOT TRUE;
EXPLAIN SELECT * FROM sa WHERE (id, k) NOT IN (SELECT id, k FROM sb WHERE k = 3);
EXPLAIN SELECT * FROM sa WHERE id NOT IN (SELECT id FROM sb);
EXPLAIN SELECT * FROM sa WHERE id <> ALL (SELECT id FROM sb WHERE k = 3);
EXPLAIN SELECT k FROM sa WHERE id NOT IN (SELECT id FROM sb WHERE k = 3) ORDER BY k LIMIT 5;
EXPLAIN SELECT * FROM sa WHERE id > 990 AND id NOT IN (SELECT id FROM sb WHERE k = 3);
EXPLAIN SELECT * FROM sa WHERE (id IN (SELECT id FROM sb WHERE k = 3)) IS NOT FALSE;
EXPLAIN SELECT * FROM sa WHERE (id, k) <> ALL (SELECT id, k FROM sb WHERE k = 3);
EXPLAIN (ANALYZE, BUFFERS OFF, TIMING OFF, SUMMARY OFF)
  SELECT * FROM sa WHERE id NOT IN (SELECT id FROM sb WHERE k = 3);
EXPLAIN (ANALYZE, BUFFERS OFF, TIMING OFF, SUMMARY OFF)
  SELECT * FROM sa WHERE id IN (SELECT id FROM sb WHERE k = 3) OR k = 5;
SELECT count(*) FROM sa WHERE id NOT IN (SELECT id FROM sb WHERE k = 3);
SELECT count(*) FROM sa WHERE (id, k) NOT IN (SELECT id, k FROM sb WHERE k = 3);
