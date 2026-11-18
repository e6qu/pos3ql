-- Conditions over several relations compared with a constant are join
-- filters estimated as eqjoinsel without statistics (DEFAULT_NUM_DISTINCT);
-- a COALESCE no full join clause matches (its operands in the other order)
-- filters the full join's rows. A relation with a statistics object joins
-- (its parameterized scans' estimates use no statistics object, as
-- find_single_rel_for_clauses finds no single relation), and EXPLAIN
-- VERBOSE prints the join's target lists under grouping and windows.
CREATE TABLE ga889 (x int, k int);
CREATE TABLE gb889 (y int, k int);
INSERT INTO ga889 SELECT g, g % 50 FROM generate_series(1, 600) g;
INSERT INTO gb889 SELECT g % 40, g FROM generate_series(1, 300) g;
CREATE TABLE fa889 (k int, a int);
CREATE TABLE fb889 (k int, b int);
INSERT INTO fa889 SELECT g, g % 3 FROM generate_series(1, 400) g;
INSERT INTO fb889 SELECT g * 2, g % 4 FROM generate_series(1, 300) g;
ANALYZE ga889, gb889, fa889, fb889;
EXPLAIN SELECT count(*) FROM ga889, gb889 WHERE ga889.x + gb889.y = 10;
EXPLAIN SELECT count(*) FROM ga889, gb889 WHERE ga889.x + gb889.y < 100 AND ga889.k = gb889.k;
SELECT count(*) FROM ga889, gb889 WHERE ga889.x + gb889.y = 10;
SELECT count(*) FROM ga889, gb889 WHERE ga889.x + gb889.y < 100 AND ga889.k = gb889.k;
EXPLAIN SELECT * FROM fa889 FULL JOIN fb889 ON fa889.k = fb889.k WHERE COALESCE(fb889.k, fa889.k) = 30;
EXPLAIN SELECT * FROM fa889 FULL JOIN fb889 ON fa889.k = fb889.k WHERE COALESCE(fa889.k, fb889.k, 0) = 30;
EXPLAIN SELECT * FROM fa889 FULL JOIN fb889 ON fa889.k = fb889.k WHERE COALESCE(fa889.a, fb889.b) = 1;
SELECT * FROM fa889 FULL JOIN fb889 ON fa889.k = fb889.k WHERE COALESCE(fb889.k, fa889.k) = 30;
SELECT count(*) FROM fa889 FULL JOIN fb889 ON fa889.k = fb889.k WHERE COALESCE(fa889.a, fb889.b) = 1;
CREATE TABLE se889 (a int, b int);
CREATE TABLE sf889 (k int, w int);
INSERT INTO se889 SELECT g, g % 9 FROM generate_series(1, 800) g;
INSERT INTO sf889 SELECT g, g % 11 FROM generate_series(1, 300) g;
CREATE STATISTICS se889_s ON (a % 40) FROM se889;
CREATE STATISTICS se889_d (dependencies, ndistinct) ON a, b FROM se889;
CREATE INDEX se889_a ON se889 (a);
ANALYZE se889, sf889;
EXPLAIN SELECT * FROM se889 JOIN sf889 ON se889.a % 40 = sf889.w;
EXPLAIN SELECT * FROM se889 JOIN sf889 ON se889.a % 40 = sf889.k;
EXPLAIN SELECT * FROM se889 JOIN sf889 ON se889.a % 7 = sf889.k;
EXPLAIN SELECT * FROM sf889 JOIN se889 ON se889.a = sf889.k WHERE se889.b = 3 AND sf889.w = 2;
SELECT count(*) FROM se889 JOIN sf889 ON se889.a % 40 = sf889.w;
SELECT count(*) FROM sf889 JOIN se889 ON se889.a = sf889.k WHERE se889.b = 3 AND sf889.w = 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT ga889.k, count(*), max(gb889.y) FROM ga889 JOIN gb889 ON ga889.k = gb889.k GROUP BY ga889.k;
EXPLAIN (VERBOSE, COSTS OFF) SELECT ga889.k, sum(gb889.y) FROM ga889 JOIN gb889 ON ga889.k = gb889.k GROUP BY ga889.k HAVING count(*) > 2 ORDER BY 2 LIMIT 4;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM ga889 JOIN gb889 ON ga889.k = gb889.k;
EXPLAIN (VERBOSE, COSTS OFF) SELECT ga889.x, rank() OVER (PARTITION BY gb889.y ORDER BY ga889.k) FROM ga889 JOIN gb889 ON ga889.k = gb889.k;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT ga889.x FROM ga889 JOIN gb889 ON ga889.k = gb889.k;
DROP TABLE ga889, gb889, fa889, fb889, se889, sf889;
