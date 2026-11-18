-- A bitmap OR, and an index scan whose conditions come from an OR, estimate
-- each arm's conditions through the statistics objects of the table
-- (`generate_bitmap_or_paths` calls `clauselist_selectivity` on each arm).
CREATE TABLE o897 (a integer, b integer, c integer);
INSERT INTO o897 SELECT g % 100, g % 100, g FROM generate_series(1, 6000) g;
CREATE INDEX o897_a ON o897 (a);
CREATE INDEX o897_b ON o897 (b);
CREATE INDEX o897_ab ON o897 (a, b);
CREATE STATISTICS s897 (dependencies, mcv, ndistinct) ON a, b FROM o897;
ANALYZE o897;
EXPLAIN SELECT * FROM o897 WHERE (a = 1 AND b = 1) OR (a = 2 AND b = 2);
EXPLAIN SELECT * FROM o897 WHERE (a = 1 AND b = 2) OR (a = 3 AND b = 3);
EXPLAIN SELECT * FROM o897 WHERE a = 1 AND b = 1;
EXPLAIN SELECT * FROM o897 WHERE a = 1 AND b = 2;
EXPLAIN SELECT * FROM o897 WHERE (a = 1 OR a = 2) AND b = 1;
EXPLAIN SELECT * FROM o897 WHERE (a = 1 AND b = 1 AND c < 100) OR (a = 2 AND b = 2);
EXPLAIN SELECT * FROM o897 WHERE a = 1 OR b = 1;
EXPLAIN SELECT * FROM o897 WHERE a IN (1, 2) AND b IN (1, 5);
EXPLAIN SELECT c FROM o897 WHERE (a = 5 AND b = 5) OR (a = 6 AND b = 7) ORDER BY a;
EXPLAIN SELECT count(*) FROM o897 WHERE (a = 1 AND b = 1) OR (a = 2 AND b = 2);
EXPLAIN SELECT * FROM o897 WHERE (a = 1 OR a = 2 OR a = 3) AND b IN (1, 2, 3);
EXPLAIN SELECT * FROM o897 WHERE (a = 1 OR a = 2) AND b = 2;
EXPLAIN SELECT * FROM o897 WHERE (a < 2 OR a > 97) AND b < 5;
EXPLAIN SELECT * FROM o897 WHERE (b = 7 OR b = 8) AND a = 7 AND c < 3000;
SELECT count(*) FROM o897 WHERE (a = 1 OR a = 2) AND b = 1;
