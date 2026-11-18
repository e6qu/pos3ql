-- Upper stages over a join (grouping_planner): HAVING with aggregates
-- (cost_agg's quals), ORDER BY of expressions and aggregates (their own
-- equivalence classes), computed select-list expressions above a grouping
-- (create_agg_path's target cost), a LIMIT above a plain aggregate, and a
-- sorted grouping feeding order-dependent aggregates in its sort's order.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/pathnode.c
CREATE TABLE ha879 (id int, k int, v int, t text);
CREATE TABLE hb879 (id int, k int, w int, t text);
INSERT INTO ha879 SELECT g, g % 40, g % 7, 'a' || (g % 11) FROM generate_series(1, 400) g;
INSERT INTO hb879 SELECT g, g % 50, g % 3, 'w' || (g % 9) FROM generate_series(1, 300) g;
ANALYZE ha879;
ANALYZE hb879;
EXPLAIN SELECT ha879.k, count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.k HAVING count(*) > 70;
SELECT ha879.k, count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.k HAVING count(*) > 70;
EXPLAIN SELECT ha879.v, sum(hb879.w) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.v HAVING sum(hb879.w) <> 300 AND ha879.v > 1;
SELECT ha879.v, sum(hb879.w) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.v HAVING sum(hb879.w) <> 300 AND ha879.v > 1;
EXPLAIN SELECT ha879.k, count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.k ORDER BY count(*) DESC, ha879.k LIMIT 5;
SELECT ha879.k, count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.k ORDER BY count(*) DESC, ha879.k LIMIT 5;
EXPLAIN SELECT count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.k ORDER BY sum(hb879.id), 1;
SELECT count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.k ORDER BY sum(hb879.id), 1;
EXPLAIN SELECT ha879.id, hb879.id FROM ha879 JOIN hb879 ON ha879.k = hb879.k ORDER BY ha879.v + hb879.w, ha879.id, hb879.id LIMIT 5;
SELECT ha879.id, hb879.id FROM ha879 JOIN hb879 ON ha879.k = hb879.k ORDER BY ha879.v + hb879.w, ha879.id, hb879.id LIMIT 5;
EXPLAIN SELECT ha879.id, ha879.v * 2 AS d FROM ha879 JOIN hb879 ON ha879.k = hb879.k WHERE hb879.id < 30 ORDER BY d DESC, 1;
SELECT ha879.id, ha879.v * 2 AS d FROM ha879 JOIN hb879 ON ha879.k = hb879.k WHERE hb879.id < 30 ORDER BY d DESC, 1;
EXPLAIN SELECT ha879.k + 1, count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.k;
SELECT ha879.k + 1, count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.k;
EXPLAIN SELECT ha879.v, count(*) * 2, max(hb879.id) - min(hb879.id) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.v ORDER BY 1;
SELECT ha879.v, count(*) * 2, max(hb879.id) - min(hb879.id) FROM ha879 JOIN hb879 ON ha879.k = hb879.k GROUP BY ha879.v ORDER BY 1;
EXPLAIN SELECT count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k LIMIT 1;
SELECT count(*) FROM ha879 JOIN hb879 ON ha879.k = hb879.k LIMIT 1;
EXPLAIN SELECT count(*) + 1, sum(hb879.w) FROM ha879 JOIN hb879 ON ha879.k = hb879.k HAVING count(*) > 1 ORDER BY 2 LIMIT 3;
SELECT count(*) + 1, sum(hb879.w) FROM ha879 JOIN hb879 ON ha879.k = hb879.k HAVING count(*) > 1 ORDER BY 2 LIMIT 3;
SET enable_hashagg = off;
EXPLAIN SELECT ha879.v, string_agg(hb879.t, ',') FROM ha879 JOIN hb879 ON ha879.k = hb879.k WHERE ha879.id < 30 GROUP BY ha879.v;
SELECT ha879.v, string_agg(hb879.t, ',') FROM ha879 JOIN hb879 ON ha879.k = hb879.k WHERE ha879.id < 30 GROUP BY ha879.v;
EXPLAIN SELECT ha879.t, array_agg(hb879.id) FROM ha879 JOIN hb879 ON ha879.k = hb879.k WHERE hb879.w = 1 AND ha879.id < 60 GROUP BY ha879.t ORDER BY ha879.t DESC;
SELECT ha879.t, array_agg(hb879.id) FROM ha879 JOIN hb879 ON ha879.k = hb879.k WHERE hb879.w = 1 AND ha879.id < 60 GROUP BY ha879.t ORDER BY ha879.t DESC;
RESET enable_hashagg;
EXPLAIN SELECT ha879.v, string_agg(hb879.t, ',') FROM ha879 JOIN hb879 ON ha879.k = hb879.k WHERE ha879.id < 30 GROUP BY ha879.v;
SELECT ha879.v, string_agg(hb879.t, ',') FROM ha879 JOIN hb879 ON ha879.k = hb879.k WHERE ha879.id < 30 GROUP BY ha879.v;
