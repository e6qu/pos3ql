-- Aggregates with DISTINCT or ORDER BY inputs: PostgreSQL 16+ extends the
-- grouping's pathkeys by the ordered aggregates' keys that most of them
-- share (adjust_group_pathkeys_for_groupagg), so a GroupAggregate or plain
-- Aggregate reads rows sorted by them and never hashes.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
CREATE TABLE p887 (a int, b int, c text);
INSERT INTO p887 SELECT g, g % 10, 'x' || (g % 7) FROM generate_series(1, 1000) g;
CREATE TABLE q887 (k int PRIMARY KEY, v int);
INSERT INTO q887 SELECT g, g % 13 FROM generate_series(1, 300) g;
ANALYZE p887;
ANALYZE q887;
EXPLAIN SELECT string_agg(c, ',' ORDER BY c) FROM p887;
EXPLAIN SELECT b, array_agg(a ORDER BY a DESC) FROM p887 GROUP BY b;
EXPLAIN SELECT b, count(DISTINCT c) FROM p887 GROUP BY b;
EXPLAIN SELECT b, count(DISTINCT c), sum(a) FROM p887 GROUP BY b ORDER BY b;
EXPLAIN SELECT q887.v, array_agg(p887.a ORDER BY p887.a) FROM p887 JOIN q887 ON p887.a = q887.k GROUP BY q887.v;
EXPLAIN SELECT count(DISTINCT q887.v) FROM p887 JOIN q887 ON p887.a = q887.k;
EXPLAIN SELECT q887.v, string_agg(p887.c, ',' ORDER BY p887.c), count(DISTINCT p887.b) FROM p887 JOIN q887 ON p887.a = q887.k GROUP BY q887.v;
SELECT string_agg(c, ',' ORDER BY c DESC) FROM p887 WHERE a < 20;
SELECT b, array_agg(a ORDER BY a DESC) FROM p887 WHERE a < 40 GROUP BY b ORDER BY b;
SELECT b, count(DISTINCT c) FROM p887 GROUP BY b ORDER BY b;
SELECT q887.v, array_agg(p887.a ORDER BY p887.a) FROM p887 JOIN q887 ON p887.a = q887.k WHERE q887.k < 60 GROUP BY q887.v ORDER BY q887.v;
SELECT q887.v, string_agg(p887.c, ',' ORDER BY p887.c), count(DISTINCT p887.b) FROM p887 JOIN q887 ON p887.a = q887.k WHERE q887.k < 50 GROUP BY q887.v ORDER BY q887.v;
