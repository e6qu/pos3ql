-- A LIKE pattern's fixed prefix becomes index conditions, as
-- match_pattern_prefix builds them: a range from the prefix to its greater
-- string under C collation (pattern operators for a pattern class), or
-- equality for a pattern without wildcards; the LIKE stays a filter. Its
-- selectivity is patternsel's: the histogram entries the pattern matches,
-- the prefix's range and the rest of the pattern, and the most common
-- values it matches. A varchar column cast to text is the column. The rows
-- stay under the forced-spill run's 256 KiB memtable.
CREATE TABLE l731 (id int PRIMARY KEY, b text, v varchar(20), t text);
INSERT INTO l731
  SELECT g, 'item' || (g % 500), 'v' || g,
         CASE WHEN g % 4 = 0 THEN 'common' ELSE 'tag' || (g % 97) END
  FROM generate_series(1, 3000) g;
CREATE INDEX l731_b ON l731 (b);
CREATE INDEX l731_v ON l731 (v varchar_pattern_ops);
CREATE INDEX l731_t ON l731 (t text_pattern_ops);
ANALYZE l731;
EXPLAIN SELECT * FROM l731 WHERE b LIKE 'item12%';
EXPLAIN SELECT * FROM l731 WHERE b LIKE 'item123%';
EXPLAIN SELECT * FROM l731 WHERE b LIKE 'item12';
EXPLAIN SELECT * FROM l731 WHERE b LIKE 'item1_3';
EXPLAIN SELECT * FROM l731 WHERE b LIKE 'item4%' ORDER BY b LIMIT 3;
EXPLAIN SELECT * FROM l731 WHERE b LIKE '%12';
EXPLAIN SELECT * FROM l731 WHERE b NOT LIKE 'item12%';
EXPLAIN SELECT * FROM l731 WHERE v LIKE 'v12%';
EXPLAIN SELECT * FROM l731 WHERE v::text = 'v7';
EXPLAIN SELECT * FROM l731 WHERE t LIKE 'tag1%';
EXPLAIN SELECT * FROM l731 WHERE t LIKE 'comm%';
EXPLAIN SELECT * FROM l731 WHERE t LIKE 'tag5%' AND id < 100;
SELECT id, b FROM l731 WHERE b LIKE 'item4%' ORDER BY b LIMIT 3;
SELECT count(*) FROM l731 WHERE b LIKE 'item1_3';
SELECT id FROM l731 WHERE v LIKE 'v12%' ORDER BY id LIMIT 5;
SELECT id FROM l731 WHERE v::text = 'v7';
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM l731 WHERE b LIKE 'item1_3';
:a SELECT * FROM l731 WHERE b LIKE 'item4%' ORDER BY b LIMIT 3;
:a SELECT * FROM l731 WHERE v LIKE 'v29%';
