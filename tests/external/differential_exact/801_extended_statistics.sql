-- ANALYZE builds a statistics object's ndistinct (`statext_ndistinct_build`),
-- functional dependencies (`statext_dependencies_build`) and MCV list
-- (`statext_mcv_build`) over its sample as PostgreSQL does, and pg_stats_ext
-- shows them; the planner estimates conditions on two or more keys from the
-- MCV list, then from the dependencies (`statext_clauselist_selectivity`).
CREATE TABLE s801 (a int, b int, c int, d text);
INSERT INTO s801 SELECT g % 20, g % 10, g % 7, 'v' || (g % 5) FROM generate_series(1, 700) g;
INSERT INTO s801 VALUES (NULL, NULL, 1, NULL), (1, NULL, NULL, 'x');
CREATE STATISTICS s801_all ON a, b, c FROM s801;
CREATE STATISTICS s801_mcv (mcv) ON c, a FROM s801;
CREATE STATISTICS s801_deps (dependencies, ndistinct) ON d, b, a, c FROM s801;
CREATE STATISTICS s801_expr ON (a + b), (d || 'z'), c FROM s801;
ANALYZE s801;
SELECT statistics_name, attnames, exprs, kinds, n_distinct, dependencies
  FROM pg_stats_ext WHERE tablename = 's801' ORDER BY 1;
SELECT statistics_name, most_common_vals, most_common_val_nulls, most_common_freqs,
       most_common_base_freqs
  FROM pg_stats_ext WHERE tablename = 's801' ORDER BY 1;
SELECT s.stxname, d.stxdndistinct, d.stxddependencies
  FROM pg_statistic_ext_data d JOIN pg_statistic_ext s ON s.oid = d.stxoid
  WHERE s.stxname LIKE 's801%' ORDER BY 1;
-- Ties among equal counts keep PostgreSQL's quicksort order; a value wider
-- than 1024 bytes leaves its row out of the MCV list and the dependencies.
CREATE TABLE m801 (a int, b text, c numeric, d date);
INSERT INTO m801 SELECT g % 13,
    CASE WHEN g % 3 = 0 THEN 'hello world' WHEN g % 3 = 1 THEN NULL ELSE 'x,y' END,
    (g % 4) / 3.0, date '2020-01-01' + (g % 6)
  FROM generate_series(1, 997) g;
INSERT INTO m801 SELECT 1, repeat('w', 2000), 1, NULL FROM generate_series(1, 5);
CREATE STATISTICS m801_abc (mcv, ndistinct, dependencies) ON a, b, c FROM m801;
CREATE STATISTICS m801_bd (mcv) ON b, d FROM m801;
CREATE STATISTICS m801_ad (mcv) ON a, d FROM m801;
ALTER STATISTICS m801_ad SET STATISTICS 10;
ANALYZE m801;
SELECT statistics_name, n_distinct, dependencies
  FROM pg_stats_ext WHERE tablename = 'm801' ORDER BY 1;
SELECT statistics_name, most_common_vals, most_common_val_nulls, most_common_freqs,
       most_common_base_freqs
  FROM pg_stats_ext WHERE tablename = 'm801' ORDER BY 1;
-- A uniform distribution keeps no MCV list.
CREATE TABLE u801 (x int, y int);
INSERT INTO u801 SELECT g, g FROM generate_series(1, 300) g;
CREATE STATISTICS u801_xy ON x, y FROM u801;
ANALYZE u801;
SELECT statistics_name, n_distinct, dependencies, most_common_vals
  FROM pg_stats_ext WHERE tablename = 'u801';
-- Selectivity: independent columns, then dependencies, then an MCV list.
CREATE TABLE p801 (a int, b int, c int);
INSERT INTO p801 SELECT g % 100, g % 50, g % 7 FROM generate_series(1, 3000) g;
CREATE INDEX p801_a ON p801 (a);
ANALYZE p801;
EXPLAIN SELECT * FROM p801 WHERE a = 1 AND b = 1;
CREATE STATISTICS p801_deps (dependencies) ON a, b FROM p801;
ANALYZE p801;
EXPLAIN SELECT * FROM p801 WHERE a = 1 AND b = 1;
EXPLAIN SELECT * FROM p801 WHERE a = 1 AND b = 1 AND c = 2;
EXPLAIN SELECT * FROM p801 WHERE a IN (1, 2) AND b = 1;
CREATE STATISTICS p801_mcv (mcv) ON a, b, c FROM p801;
ANALYZE p801;
EXPLAIN SELECT * FROM p801 WHERE a = 1 AND b = 1;
EXPLAIN SELECT * FROM p801 WHERE a = 1 AND b = 2;
EXPLAIN SELECT * FROM p801 WHERE a < 5 AND b > 1 AND c = 3;
EXPLAIN SELECT * FROM p801 WHERE (a = 1 OR a = 2) AND b = 1;
EXPLAIN SELECT * FROM p801 WHERE a = 1 AND b IS NULL;
DROP TABLE s801, m801, u801, p801;
