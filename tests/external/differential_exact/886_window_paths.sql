-- Window functions planned as PostgreSQL 18 plans them over one relation
-- and over joins: create_window_paths over the cheapest path and the paths
-- presorted by the first window's keys, each window sorting (fully or
-- incrementally) only when its input is not already ordered, costed by
-- cost_windowagg with get_windowclause_startup_tuples, then the query's
-- sort and limit.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/costsize.c
CREATE TABLE w886 (a int, b int, c text);
INSERT INTO w886 SELECT g, g % 10, 'x' || (g % 7) FROM generate_series(1, 1000) g;
CREATE INDEX w886_a ON w886 (a);
CREATE TABLE v886 (k int PRIMARY KEY, v int);
INSERT INTO v886 SELECT g, g % 13 FROM generate_series(1, 300) g;
ANALYZE w886;
ANALYZE v886;
EXPLAIN SELECT a, sum(b) OVER (PARTITION BY b ORDER BY a) FROM w886;
EXPLAIN SELECT a, row_number() OVER (ORDER BY a) FROM w886 LIMIT 5;
EXPLAIN SELECT a, rank() OVER (PARTITION BY c ORDER BY b), sum(a) OVER (PARTITION BY b) FROM w886 ORDER BY b;
EXPLAIN SELECT a, count(*) OVER () FROM w886 WHERE a < 50;
EXPLAIN SELECT a, avg(a) OVER (ORDER BY a ROWS BETWEEN 2 PRECEDING AND 3 FOLLOWING) FROM w886;
EXPLAIN SELECT a, sum(a) OVER (PARTITION BY b ORDER BY a RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM w886 ORDER BY a;
EXPLAIN SELECT a, b, row_number() OVER (PARTITION BY a ORDER BY b) FROM w886 ORDER BY a, b DESC;
EXPLAIN SELECT w886.a, v886.v, rank() OVER (PARTITION BY v886.v ORDER BY w886.a) FROM w886 JOIN v886 ON w886.a = v886.k;
EXPLAIN SELECT w886.a, row_number() OVER (ORDER BY w886.a) FROM w886 JOIN v886 ON w886.a = v886.k ORDER BY w886.a LIMIT 10;
SELECT a, sum(b) OVER (PARTITION BY b ORDER BY a) FROM w886 ORDER BY a LIMIT 12;
SELECT a, row_number() OVER (ORDER BY a) FROM w886 LIMIT 5;
SELECT w886.a, v886.v, rank() OVER (PARTITION BY v886.v ORDER BY w886.a) FROM w886 JOIN v886 ON w886.a = v886.k ORDER BY w886.a LIMIT 12;
EXPLAIN SELECT v886.v, count(*), rank() OVER (ORDER BY v886.v DESC) FROM w886 JOIN v886 ON w886.a = v886.k GROUP BY v886.v;
EXPLAIN SELECT v886.v, sum(w886.b), sum(sum(w886.b)) OVER (ORDER BY v886.v) FROM w886 JOIN v886 ON w886.a = v886.k GROUP BY v886.v ORDER BY v886.v;
EXPLAIN SELECT w886.b, v886.v, row_number() OVER (PARTITION BY w886.b ORDER BY v886.v), lag(v886.v) OVER (PARTITION BY w886.b ORDER BY v886.v) FROM w886 JOIN v886 ON w886.a = v886.k;
EXPLAIN SELECT w886.b, first_value(w886.a) OVER w, last_value(w886.a) OVER w FROM w886 JOIN v886 ON w886.a = v886.k WINDOW w AS (PARTITION BY w886.b ORDER BY w886.a ROWS BETWEEN 1 PRECEDING AND 2 FOLLOWING) ORDER BY w886.b, w886.a LIMIT 7;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, sum(b) OVER (PARTITION BY b ORDER BY a) FROM w886;
SELECT v886.v, count(*), rank() OVER (ORDER BY v886.v DESC) FROM w886 JOIN v886 ON w886.a = v886.k GROUP BY v886.v ORDER BY v886.v;
SELECT v886.v, sum(w886.b), sum(sum(w886.b)) OVER (ORDER BY v886.v) FROM w886 JOIN v886 ON w886.a = v886.k GROUP BY v886.v ORDER BY v886.v;
SELECT w886.b, first_value(w886.a) OVER w, last_value(w886.a) OVER w FROM w886 JOIN v886 ON w886.a = v886.k WINDOW w AS (PARTITION BY w886.b ORDER BY w886.a ROWS BETWEEN 1 PRECEDING AND 2 FOLLOWING) ORDER BY w886.b, w886.a LIMIT 7;
