-- Pattern conditions as like_support.c reads them under the C collation:
-- LIKE with an ESCAPE folds through like_escape; ILIKE's fixed prefix stops
-- at a letter; a regular expression's prefix comes from its anchored
-- characters (regex_fixed_prefix), exact with a final $; starts_with is a
-- prefix pattern through its support function; a non-ASCII prefix bounds
-- the range with make_greater_string's UTF-8 increment. Each condition's
-- selectivity is patternsel's. The rows stay under the forced-spill run's
-- 256 KiB memtable.
CREATE TABLE s752 (id int PRIMARY KEY, b text, n int);
INSERT INTO s752 SELECT g, 'k' || (g % 300) || 'é' || g, g % 50 FROM generate_series(1, 3000) g;
CREATE INDEX ON s752 (b);
ANALYZE s752;
\set e 'EXPLAIN'
:e SELECT * FROM s752 WHERE b LIKE 'k1#_%' ESCAPE '#';
:e SELECT * FROM s752 WHERE b LIKE 'k12é%';
:e SELECT * FROM s752 WHERE b LIKE 'k12é1';
:e SELECT * FROM s752 WHERE b ILIKE 'k12%';
:e SELECT * FROM s752 WHERE b ILIKE '12%';
:e SELECT * FROM s752 WHERE b ~ '^k12';
:e SELECT * FROM s752 WHERE b ~ '^k12é1$';
:e SELECT * FROM s752 WHERE b ~* '^k12';
:e SELECT * FROM s752 WHERE b ~ 'k12';
:e SELECT * FROM s752 WHERE b ~ '^k1(2|3)';
:e SELECT * FROM s752 WHERE b ~ '^k(12|13)';
:e SELECT * FROM s752 WHERE b ~ '^k1+2';
:e SELECT * FROM s752 WHERE b ~ '^k1{2}';
:e SELECT * FROM s752 WHERE b !~ '^k12';
:e SELECT * FROM s752 WHERE b NOT ILIKE 'k%';
:e SELECT * FROM s752 WHERE starts_with(b, 'k1');
:e SELECT * FROM s752 WHERE starts_with(b, 'k12é1');
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM s752 WHERE b LIKE 'k12é%';
:a SELECT * FROM s752 WHERE b ~ '^k12é1$';
SELECT count(*) FROM s752 WHERE b ~ '^k12';
SELECT id FROM s752 WHERE b LIKE 'k12é%' ORDER BY id LIMIT 3;
SELECT id, b FROM s752 WHERE b LIKE 'k1#_%' ESCAPE '#';
SELECT id FROM s752 WHERE b ~ '^k1(2|3)é' ORDER BY id LIMIT 4;
SELECT count(*) FROM s752 WHERE starts_with(b, 'k12é1');
