-- PostgreSQL 18 explain.c show_scan_qual and createplan.c qual placement
-- are the reference: an index scan prints the conditions its index
-- consumes as Index Cond, with the indexed column unqualified and first,
-- and outer joins keep each ON or WHERE condition where PostgreSQL does.
CREATE TABLE t374 (a int PRIMARY KEY, b int, c text);
CREATE TABLE s374 (x int, y int);
INSERT INTO t374 SELECT g, g % 50, 'v' || g FROM generate_series(1, 3000) g;
INSERT INTO s374 SELECT g, g FROM generate_series(1, 3) g;
ANALYZE t374;
ANALYZE s374;
EXPLAIN (COSTS OFF) SELECT * FROM t374 WHERE a = 5;
EXPLAIN (COSTS OFF) SELECT * FROM t374 WHERE 5 = a AND c <> 'x';
EXPLAIN (COSTS OFF) SELECT * FROM t374 t WHERE t.a = 6 AND t.b > 1;
EXPLAIN (COSTS OFF) UPDATE t374 SET c = 'u' WHERE a = 7;
EXPLAIN (COSTS OFF) DELETE FROM t374 WHERE a = 8 AND b > 1;
EXPLAIN (COSTS OFF) SELECT * FROM s374 LEFT JOIN t374 ON s374.x = t374.a AND t374.b < s374.y;
EXPLAIN (COSTS OFF) SELECT * FROM s374 LEFT JOIN t374 ON t374.a = s374.x AND t374.c <> 'z' AND s374.y > 1 WHERE s374.x < 3;
EXPLAIN (COSTS OFF) SELECT * FROM s374 LEFT JOIN s374 s2 ON s2.x = s374.x AND s2.y > s374.y;
EXPLAIN (COSTS OFF) SELECT * FROM s374 LEFT JOIN s374 s2 ON s2.x = s374.x WHERE s2.y IS NULL;
EXPLAIN (COSTS OFF) SELECT * FROM s374 LEFT JOIN s374 s2 ON s2.x = s374.x AND s2.y > 1 WHERE s2.y IS NULL OR s374.y > 1;
