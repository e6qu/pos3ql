-- A btree pattern operator class serves `=` and the `~<~` family only; a
-- default class serves the collation-aware comparisons only.
CREATE TABLE words (id int, w text);
INSERT INTO words SELECT g, 'w' || g FROM generate_series(1, 2000) g;
CREATE INDEX words_w ON words (w text_pattern_ops);
ANALYZE words;
EXPLAIN (COSTS OFF) SELECT w FROM words WHERE w < 'w100';
EXPLAIN (COSTS OFF) SELECT w FROM words WHERE w >= 'w1999';
SELECT w FROM words WHERE w >= 'w1998' AND w < 'w2' ORDER BY w;
SELECT w FROM words WHERE w ~>=~ 'w1998' AND w ~<~ 'w2' ORDER BY w;
DROP INDEX words_w;
CREATE INDEX words_d ON words (w);
EXPLAIN (COSTS OFF) SELECT w FROM words WHERE w ~>=~ 'w1999';
SELECT w FROM words WHERE w ~>=~ 'w1998' AND w ~<~ 'w2' ORDER BY w;
