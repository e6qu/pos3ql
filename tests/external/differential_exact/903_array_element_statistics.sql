-- The rows of each statement stay under the forced-spill run's 256 KiB memtable.
-- ANALYZE of an array column or statistics expression keeps the most common
-- elements, their frequencies and the histogram of distinct element counts
-- (`compute_array_stats`).
CREATE TABLE r903 (id integer, tags text[], nums integer[], mixed text[]);
INSERT INTO r903
SELECT g,
       CASE WHEN g % 11 = 0 THEN NULL ELSE ARRAY['t' || (g % 7), 'u' || (g % 3), 't' || (g % 7)] END,
       ARRAY[g % 5, g % 13, g % 5, g % 2],
       CASE WHEN g % 4 = 0 THEN ARRAY['a', NULL, 'b'] WHEN g % 4 = 1 THEN '{}' ELSE ARRAY['c' || (g % 9)] END
FROM generate_series(1, 500) g;
CREATE STATISTICS s903 ON (tags[1]), (nums || ARRAY[1]), (mixed || ARRAY['z']) FROM r903;
ANALYZE r903;
SELECT attname, null_frac, avg_width, n_distinct, most_common_elems, most_common_elem_freqs, elem_count_histogram
FROM pg_stats WHERE tablename = 'r903' AND attname <> 'id' ORDER BY attname;
SELECT expr, most_common_elems, most_common_elem_freqs, elem_count_histogram
FROM pg_stats_ext_exprs WHERE tablename = 'r903' ORDER BY expr;
