-- A partitioned table pruning leaves no partition is PostgreSQL's dummy
-- relation. On an outer join's nullable side it is a Result returning no
-- rows, hashed for an equality join (a nested loop would pay a tuple per
-- outer row) and rescanned directly otherwise; the plan scans none of its
-- range table entries, so its columns print unnamed. An inner or semi join
-- with it returns no rows at all, and a plan scanning nothing names it.
CREATE TABLE pt613 (id int, v int) PARTITION BY RANGE (id);
CREATE TABLE pt613_1 PARTITION OF pt613 FOR VALUES FROM (1) TO (10);
CREATE TABLE pt613_2 PARTITION OF pt613 FOR VALUES FROM (10) TO (20);
INSERT INTO pt613 SELECT g, g FROM generate_series(1, 19) g;
CREATE TABLE o613 (k int, w int);
INSERT INTO o613 VALUES (5, 1), (15, 2);
EXPLAIN (COSTS OFF) SELECT * FROM o613 LEFT JOIN pt613 ON pt613.id = o613.k AND pt613.id > 100;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM o613 LEFT JOIN pt613 ON pt613.id = o613.k AND pt613.id > 100;
EXPLAIN (COSTS OFF) SELECT o613.k FROM o613 LEFT JOIN pt613 ON pt613.id = o613.k AND pt613.id > 100;
EXPLAIN (COSTS OFF) SELECT * FROM pt613 RIGHT JOIN o613 ON pt613.id = o613.k AND pt613.id > 100;
EXPLAIN (COSTS OFF) SELECT * FROM o613 LEFT JOIN pt613 ON pt613.v = o613.w AND pt613.id < 0;
EXPLAIN (COSTS OFF) SELECT * FROM o613 LEFT JOIN pt613 ON pt613.v > o613.w AND pt613.id < 0;
EXPLAIN (COSTS OFF) SELECT * FROM o613 WHERE EXISTS (SELECT 1 FROM pt613 WHERE pt613.id = o613.k AND pt613.id > 100);
EXPLAIN (COSTS OFF) SELECT * FROM o613 WHERE k IN (SELECT id FROM pt613 WHERE id > 100);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM pt613 WHERE id > 100;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT * FROM pt613 WHERE id > 100) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM o613 JOIN pt613 ON pt613.id = o613.k WHERE pt613.id > 100;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM pt613 WHERE id > 100;
ANALYZE o613;
EXPLAIN (COSTS OFF) SELECT * FROM o613 LEFT JOIN pt613 ON pt613.id = o613.k AND pt613.id > 100;
SELECT * FROM o613 LEFT JOIN pt613 ON pt613.id = o613.k AND pt613.id > 100 ORDER BY k;
SELECT * FROM o613 WHERE EXISTS (SELECT 1 FROM pt613 WHERE pt613.id = o613.k AND pt613.id > 100);
