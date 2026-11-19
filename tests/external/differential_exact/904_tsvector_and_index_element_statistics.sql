-- The rows of each statement stay under the forced-spill run's 256 KiB memtable.
-- A tsvector column's ANALYZE keeps its most common lexemes
-- (`compute_tsvector_stats`) and a tsvector's width is its stored size;
-- an array-valued index expression's element statistics show in pg_stats.
CREATE TABLE v904 (id integer, doc tsvector, tags text[]);
INSERT INTO v904
SELECT g,
       CASE WHEN g % 13 = 0 THEN NULL
            ELSE to_tsvector('simple', 'word' || (g % 7) || ' common ' || CASE WHEN g % 2 = 0 THEN 'even rare' || (g % 29) ELSE 'odd' END || ' word' || (g % 7))
       END,
       ARRAY['a' || (g % 4), 'b' || (g % 3)]
FROM generate_series(1, 450) g;
CREATE INDEX v904_tags ON v904 ((tags || ARRAY['x']));
ANALYZE v904;
SELECT attname, null_frac, avg_width, n_distinct, most_common_vals, histogram_bounds, correlation, most_common_elems, most_common_elem_freqs, elem_count_histogram
FROM pg_stats WHERE tablename = 'v904' AND attname = 'doc';
SELECT tablename, attname, null_frac, avg_width, n_distinct, most_common_elems, most_common_elem_freqs, elem_count_histogram
FROM pg_stats WHERE tablename = 'v904_tags';
SELECT count(*) FROM v904 WHERE doc @@ 'common';
