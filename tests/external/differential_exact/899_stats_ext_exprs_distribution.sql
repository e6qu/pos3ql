-- A statistics object's expressions keep their most common values,
-- histogram and correlation as a table column's statistics do, and
-- pg_stats_ext_exprs shows them.
CREATE TABLE e899 (a integer, b text, c numeric, d timestamp, tags text[]);
INSERT INTO e899
SELECT g % 50, 'item' || (g % 17) || repeat('x', g % 3), (g % 23) * 1.5, timestamp '2024-01-01' + (g % 400) * interval '1 hour',
       ARRAY['t' || (g % 5), 'u' || (g % 3)]
FROM generate_series(1, 3000) g;
CREATE STATISTICS s899 (mcv) ON (a + 1), lower(b), (c * 2) FROM e899;
CREATE STATISTICS s899b ON (d::date), length(b), (tags[1]) FROM e899;
ANALYZE e899;
SELECT statistics_name, expr, inherited, null_frac, avg_width, n_distinct, most_common_vals, most_common_freqs, histogram_bounds, correlation, most_common_elems, most_common_elem_freqs, elem_count_histogram
FROM pg_stats_ext_exprs WHERE tablename = 'e899' ORDER BY statistics_name, expr;
