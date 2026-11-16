-- ANALYZE computes each column's statistics as PostgreSQL's
-- compute_scalar_stats does: a table smaller than its 300 * target row
-- sample is read whole, in physical order, giving its null fraction,
-- stored width, distinct-value estimate (Haas and Stokes' estimator when
-- some values appear once), most common values, histogram bounds of the
-- remaining values and the correlation of value order with row order.
-- The rows stay under the forced-spill run's 256 KiB memtable.
\pset format unaligned
CREATE TABLE s690 (id int, b int, c text, d numeric, e float8, f bool, g date, h text, z int);
INSERT INTO s690
  SELECT g, g % 50, 'x' || g, (g % 7) * 1.5, g / 3.0, g % 3 = 0,
         date '2020-01-01' + (g % 400), CASE WHEN g % 4 = 0 THEN NULL ELSE 'h' || (g % 9) END, NULL
  FROM generate_series(1, 2000) g;
INSERT INTO s690 VALUES (3000, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
ANALYZE s690;
SELECT attname, null_frac, avg_width, n_distinct, most_common_vals, most_common_freqs,
       histogram_bounds, correlation
  FROM pg_stats WHERE tablename = 's690' ORDER BY attname;
-- Values seen once next to repeated ones: the most common values are those
-- seen more than once and the histogram spans the rest.
CREATE TABLE m690 (a int, b text, c numeric(10, 3));
INSERT INTO m690 SELECT g % 13, (g * 7919 % 1000)::text, g % 101 / 8.0 FROM generate_series(1, 1500) g;
INSERT INTO m690 SELECT 5, '5', 0 FROM generate_series(1, 300);
INSERT INTO m690 SELECT 1000 + g, 'u' || g, g FROM generate_series(1, 40) g;
ANALYZE m690;
SELECT attname, null_frac, avg_width, n_distinct, most_common_vals, most_common_freqs,
       histogram_bounds, correlation
  FROM pg_stats WHERE tablename = 'm690' ORDER BY attname;
-- A smaller statistics target keeps fewer values (its 300 * target row
-- sample still covers the table); target 0 leaves a column's statistics as
-- they were, and naming columns analyzes only those.
ALTER TABLE m690 ALTER COLUMN a SET STATISTICS 7;
ALTER TABLE m690 ALTER COLUMN c SET STATISTICS 0;
INSERT INTO m690 SELECT 7, 'q', 7 FROM generate_series(1, 200);
ANALYZE m690 (a, c);
SELECT attname, n_distinct, most_common_vals, most_common_freqs, histogram_bounds, correlation
  FROM pg_stats WHERE tablename = 'm690' ORDER BY attname;
SET default_statistics_target = 7;
ANALYZE m690;
SELECT attname, n_distinct, most_common_vals, most_common_freqs, histogram_bounds, correlation
  FROM pg_stats WHERE tablename = 'm690' ORDER BY attname;
RESET default_statistics_target;
-- Rows in descending order correlate negatively; a unique column has no
-- most common values.
CREATE TABLE r690 (k int, t text COLLATE "C");
INSERT INTO r690 SELECT 600 - g, chr(65 + g % 26) || g FROM generate_series(1, 600) g;
DELETE FROM r690 WHERE k % 10 = 0;
ANALYZE r690;
SELECT attname, null_frac, avg_width, n_distinct, most_common_vals, most_common_freqs,
       histogram_bounds, correlation
  FROM pg_stats WHERE tablename = 'r690' ORDER BY attname;
-- A single row, and an emptied table, which keeps its earlier statistics.
CREATE TABLE one690 (v int);
INSERT INTO one690 VALUES (4);
ANALYZE one690;
SELECT attname, null_frac, n_distinct, most_common_vals, histogram_bounds, correlation
  FROM pg_stats WHERE tablename = 'one690';
DELETE FROM one690;
ANALYZE one690;
SELECT attname, null_frac, n_distinct, most_common_vals, histogram_bounds, correlation
  FROM pg_stats WHERE tablename = 'one690';
-- Every gathered statistic shows as an anyarray of the column's own type:
-- enum labels, domain and composite values, and an array column's arrays
-- (an array of arrays). Range, multirange and tsvector columns get no
-- most common values and count every value distinct; a type without an
-- equality operator leaves the distinct count unknown.
CREATE TYPE t690_mood AS ENUM ('sad', 'ok', 'happy');
CREATE DOMAIN t690_pos AS int CHECK (VALUE > 0);
CREATE DOMAIN t690_ints AS int[];
CREATE DOMAIN t690_moods AS t690_mood[];
CREATE TYPE t690_pair AS (a int, b text);
CREATE TABLE k690 (e t690_mood, ea t690_mood[], ia int[], d t690_pos, da t690_pos[],
  di t690_ints, dm t690_moods, c t690_pair, ca t690_pair[], ta text[], bx box[],
  r int4range, mr int4multirange, tv tsvector, js json);
INSERT INTO k690
  SELECT (ARRAY['sad', 'ok', 'happy'])[1 + g % 3]::t690_mood,
         ARRAY[(ARRAY['sad', 'ok', 'happy'])[1 + g % 3]::t690_mood], ARRAY[g % 4, 1],
         1 + g % 5, ARRAY[1 + g % 2]::t690_pos[], ARRAY[g % 3]::t690_ints,
         ARRAY['ok']::t690_moods, ROW(g % 3, 'x y')::t690_pair,
         ARRAY[ROW(g % 2, 'q')::t690_pair], ARRAY['a b', 'c"'],
         ARRAY[box(point(g % 2, 0), point(1, 1))],
         CASE WHEN g % 2 = 0 THEN int4range(g % 3, 5) END,
         int4multirange(int4range(g % 3, 5)), ('w' || g % 3)::tsvector, '{}'::json
  FROM generate_series(1, 100) g;
ANALYZE k690;
SELECT attname, null_frac, n_distinct, most_common_vals, most_common_freqs,
       histogram_bounds, correlation
  FROM pg_stats WHERE tablename = 'k690' ORDER BY attname;
SELECT null_frac FROM pg_stats WHERE tablename = 'k690' AND attname = 'e';
