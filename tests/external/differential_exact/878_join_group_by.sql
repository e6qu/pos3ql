-- Grouping above a join (add_paths_to_grouping_rel): a HashAggregate's
-- groups in its simplehash table's order, a GroupAggregate over the join
-- sorted by the grouping keys the ORDER BY leads with
-- (preprocess_groupclause), and the sort and limit over the groups.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/include/lib/simplehash.h
CREATE TABLE ga878 (id int, k int, v int);
CREATE TABLE gb878 (id int, k int, w int, t text);
INSERT INTO ga878 SELECT g, g % 40, g % 7 FROM generate_series(1, 400) g;
INSERT INTO gb878 SELECT g, g % 50, g % 3, 'w' || (g % 9) FROM generate_series(1, 300) g;
ANALYZE ga878;
ANALYZE gb878;
EXPLAIN SELECT ga878.k, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.k;
SELECT ga878.k, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.k;
EXPLAIN SELECT ga878.v, gb878.w, count(*), sum(gb878.id) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.v, gb878.w;
SELECT ga878.v, gb878.w, count(*), sum(gb878.id) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.v, gb878.w;
EXPLAIN SELECT gb878.t, max(ga878.id) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY gb878.t ORDER BY gb878.t DESC;
SELECT gb878.t, max(ga878.id) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY gb878.t ORDER BY gb878.t DESC;
EXPLAIN SELECT ga878.k, ga878.v, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.k, ga878.v ORDER BY ga878.k LIMIT 6;
SELECT ga878.k, ga878.v, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.k, ga878.v ORDER BY ga878.k LIMIT 6;
SET enable_hashagg = off;
EXPLAIN SELECT ga878.v, count(*), min(gb878.w) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.v;
SELECT ga878.v, count(*), min(gb878.w) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.v;
EXPLAIN SELECT ga878.v, gb878.w, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.v, gb878.w ORDER BY gb878.w DESC, ga878.v;
SELECT ga878.v, gb878.w, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.v, gb878.w ORDER BY gb878.w DESC, ga878.v;
RESET enable_hashagg;
EXPLAIN SELECT ga878.k, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k WHERE gb878.w = 1 GROUP BY ga878.k ORDER BY ga878.k;
SELECT ga878.k, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k WHERE gb878.w = 1 GROUP BY ga878.k ORDER BY ga878.k;
EXPLAIN SELECT gb878.t, count(*) FROM ga878 LEFT JOIN gb878 ON ga878.k = gb878.k AND gb878.w = 2 GROUP BY gb878.t;
SELECT gb878.t, count(*) FROM ga878 LEFT JOIN gb878 ON ga878.k = gb878.k AND gb878.w = 2 GROUP BY gb878.t;
EXPLAIN SELECT ga878.k, gb878.t, count(gb878.id) FROM ga878 LEFT JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.k, gb878.t ORDER BY gb878.t, ga878.k LIMIT 10;
SELECT ga878.k, gb878.t, count(gb878.id) FROM ga878 LEFT JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.k, gb878.t ORDER BY gb878.t, ga878.k LIMIT 10;
EXPLAIN SELECT ga878.id, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.id ORDER BY ga878.id LIMIT 5;
SELECT ga878.id, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.id ORDER BY ga878.id LIMIT 5;
EXPLAIN SELECT gb878.k, avg(ga878.v) FROM ga878 JOIN gb878 ON ga878.k = gb878.k WHERE ga878.id < 50 GROUP BY gb878.k;
SELECT gb878.k, avg(ga878.v) FROM ga878 JOIN gb878 ON ga878.k = gb878.k WHERE ga878.id < 50 GROUP BY gb878.k;
SELECT ga878.k, count(*) FROM ga878 JOIN gb878 ON ga878.k = gb878.k GROUP BY ga878.k HAVING count(*) > 70 ORDER BY 1;
EXPLAIN SELECT ga878.v, string_agg(gb878.t, ',') FROM ga878 JOIN gb878 ON ga878.k = gb878.k WHERE ga878.id < 20 GROUP BY ga878.v;
SELECT ga878.v, string_agg(gb878.t, ',') FROM ga878 JOIN gb878 ON ga878.k = gb878.k WHERE ga878.id < 20 GROUP BY ga878.v;
