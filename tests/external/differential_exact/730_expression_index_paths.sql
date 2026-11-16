-- ANALYZE gathers the statistics of each index expression, as
-- compute_index_stats does: pg_stats shows them under the index's name and
-- attribute names, over the rows a partial index's predicate keeps. A
-- condition or ORDER BY on the indexed expression reads them and uses the
-- index, whose scan returns rows in the expression's order. The rows stay
-- under the forced-spill run's 256 KiB memtable.
CREATE TABLE e730 (id int PRIMARY KEY, b text, c int, d numeric);
INSERT INTO e730
  SELECT g, CASE WHEN g % 3 = 0 THEN 'Name' ELSE 'name' END || (g % 300),
         g % 50, CASE WHEN g % 11 = 0 THEN NULL ELSE g / 7 END
  FROM generate_series(1, 3000) g;
CREATE INDEX e730_lower ON e730 (lower(b));
CREATE INDEX e730_pair ON e730 ((c * 2), id) WHERE c > 10;
CREATE INDEX e730_mixed ON e730 (c, (b || 'x'));
CREATE UNIQUE INDEX e730_neg ON e730 ((-id));
CREATE INDEX e730_round ON e730 (round(d, -1));
ANALYZE e730;
SELECT tablename, attname, null_frac, avg_width, n_distinct,
       array_length(most_common_vals::text::text[], 1) AS mcv,
       most_common_freqs[1] AS top_freq,
       array_length(histogram_bounds::text::text[], 1) AS hist, correlation
  FROM pg_stats WHERE tablename LIKE 'e730%' ORDER BY tablename, attname;
SELECT histogram_bounds FROM pg_stats WHERE tablename = 'e730_lower';
SELECT most_common_vals, most_common_freqs FROM pg_stats
  WHERE tablename = 'e730_round';
EXPLAIN SELECT * FROM e730 WHERE lower(b) = 'name5';
EXPLAIN SELECT * FROM e730 WHERE lower(b) > 'name95';
EXPLAIN SELECT * FROM e730 WHERE lower(e730.b) IN ('name5', 'name7');
EXPLAIN SELECT * FROM e730 ORDER BY lower(b) LIMIT 5;
EXPLAIN SELECT id FROM e730 WHERE lower(b) < 'name100' ORDER BY lower(b) DESC;
EXPLAIN SELECT * FROM e730 WHERE c * 2 = 40 AND c > 10;
EXPLAIN SELECT * FROM e730 WHERE c > 20 ORDER BY c * 2 LIMIT 3;
EXPLAIN SELECT * FROM e730 WHERE c = 7 AND b || 'x' = 'name7x';
EXPLAIN SELECT * FROM e730 WHERE -id = -42;
EXPLAIN SELECT * FROM e730 WHERE round(d, -1) = 100;
EXPLAIN SELECT * FROM e730 WHERE round(d, -1) IS NULL;
EXPLAIN SELECT id FROM e730 WHERE id < 10 ORDER BY lower(b);
SELECT id, b FROM e730 WHERE lower(b) > 'name95' ORDER BY lower(b), id LIMIT 6;
SELECT id, b FROM e730 ORDER BY lower(b) DESC LIMIT 4;
SELECT id FROM e730 WHERE c > 20 ORDER BY c * 2 LIMIT 3;
SELECT id, d FROM e730 WHERE round(d, -1) = 100;
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM e730 WHERE lower(b) = 'name5';
:a SELECT * FROM e730 WHERE -id BETWEEN -50 AND -45;
UPDATE e730 SET d = d + 1 WHERE lower(b) = 'name5' RETURNING id, d;
-- An index created after ANALYZE has no statistics until the next one;
-- ANALYZE of chosen columns keeps them; DROP INDEX removes them.
CREATE INDEX e730_upper ON e730 (upper(b));
SELECT count(*) FROM pg_stats WHERE tablename = 'e730_upper';
EXPLAIN SELECT * FROM e730 WHERE upper(b) = 'NAME5';
ANALYZE e730 (c);
SELECT count(*) FROM pg_stats WHERE tablename LIKE 'e730%';
DROP INDEX e730_lower;
SELECT count(*) FROM pg_stats WHERE tablename LIKE 'e730%';
