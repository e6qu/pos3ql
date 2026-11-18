-- A plain aggregate over a join PostgreSQL's join search plans: the join
-- returns the columns the aggregates' arguments read (a count(*) none), and
-- an Aggregate over its cheapest path computes them (cost_agg), reading the
-- joined rows in the plan's order, which an order-sensitive aggregate shows.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/costsize.c
CREATE TABLE ga871 (id int, k int, v int, w text);
CREATE TABLE gb871 (id int, k int, v int, w text);
CREATE TABLE gc871 (id int, k int, v int, w text);
INSERT INTO ga871 SELECT g, g % 50, (g * 7) % 13, 'a' || (g % 9) FROM generate_series(1, 800) g;
INSERT INTO gb871 SELECT g, g % 300, (g * 7) % 13, 'b' || (g % 9) FROM generate_series(1, 600) g;
INSERT INTO gc871 SELECT g, CASE WHEN g % 7 = 3 THEN NULL ELSE g % 20 END, (g * 7) % 13, 'c' || (g % 9) FROM generate_series(1, 100) g;
CREATE INDEX gb871_k ON gb871 (k);
CREATE UNIQUE INDEX ga871_id ON ga871 (id);
ANALYZE ga871; ANALYZE gb871; ANALYZE gc871;
EXPLAIN SELECT count(*) FROM ga871 JOIN gb871 ON ga871.k = gb871.k;
SELECT count(*) FROM ga871 JOIN gb871 ON ga871.k = gb871.k;
EXPLAIN SELECT sum(ga871.id), max(gb871.w), count(gc871.k) FROM ga871 JOIN gb871 ON ga871.k = gb871.k LEFT JOIN gc871 ON gb871.v = gc871.v;
SELECT sum(ga871.id), max(gb871.w), count(gc871.k) FROM ga871 JOIN gb871 ON ga871.k = gb871.k LEFT JOIN gc871 ON gb871.v = gc871.v;
EXPLAIN SELECT string_agg(ga871.id::text, ',') FROM ga871 JOIN gb871 ON ga871.id = gb871.k WHERE gb871.v < 3;
SELECT string_agg(ga871.id::text, ',') FROM ga871 JOIN gb871 ON ga871.id = gb871.k WHERE gb871.v < 3;
EXPLAIN SELECT avg(ga871.v + gb871.v) FROM ga871 JOIN gb871 ON ga871.id = gb871.k AND ga871.v = gb871.v;
SELECT avg(ga871.v + gb871.v) FROM ga871 JOIN gb871 ON ga871.id = gb871.k AND ga871.v = gb871.v;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM ga871 JOIN gb871 ON ga871.k = gb871.k;
