-- A partial index serves a query whose conditions imply its predicate
-- (predicate_implied_by: the same condition, a narrower range of the
-- column, an IN list within it, NOT NULL from a comparison); the
-- conditions its predicate implies leave the scan's filter, its entries
-- are the rows its predicate keeps, and a bitmap heap scan rechecks the
-- predicate. The rows stay under the forced-spill run's 256 KiB memtable.
CREATE TABLE p722 (id int PRIMARY KEY, k int, s text, v int);
INSERT INTO p722
  SELECT g, g % 100, CASE WHEN g % 10 = 0 THEN 'open' ELSE 'done' END,
         CASE WHEN g % 7 = 0 THEN NULL ELSE g END
  FROM generate_series(1, 3000) g;
CREATE INDEX p722_open ON p722 (k) WHERE s = 'open';
CREATE INDEX p722_small ON p722 (id) WHERE k < 10;
CREATE INDEX p722_v ON p722 (v) WHERE v IS NOT NULL;
ANALYZE p722;
\set e 'EXPLAIN (COSTS OFF)'
:e SELECT * FROM p722 WHERE s = 'open' AND k = 50;
:e SELECT * FROM p722 WHERE s = 'open';
:e SELECT * FROM p722 WHERE k = 5;
:e SELECT * FROM p722 WHERE k < 5 AND id < 300;
:e SELECT * FROM p722 WHERE k IN (1, 2) AND id < 300;
:e SELECT * FROM p722 WHERE k < 20 AND id < 300;
:e SELECT * FROM p722 WHERE v > 2990;
:e SELECT * FROM p722 WHERE v = 14;
:e SELECT * FROM p722 WHERE s = 'open' ORDER BY k LIMIT 3;
:e SELECT k FROM p722 WHERE s = 'open' AND k > 90;
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM p722 WHERE s = 'open' AND k = 50;
:a SELECT * FROM p722 WHERE k < 5 AND id < 300;
SELECT id, k FROM p722 WHERE s = 'open' AND k = 50;
SELECT id, k FROM p722 WHERE k < 5 AND id < 300;
SELECT id, k FROM p722 WHERE s = 'open' ORDER BY k LIMIT 3;
-- A boolean column tested by itself is a condition of its own, which a
-- predicate testing it is implied by.
CREATE TABLE b722 (id int PRIMARY KEY, active boolean, score int);
INSERT INTO b722 SELECT g, g % 5 = 0, g % 300 FROM generate_series(1, 3000) g;
CREATE INDEX b722_active_score ON b722 (score) WHERE active;
CREATE INDEX b722_inactive ON b722 (score) WHERE NOT active;
ANALYZE b722;
:e SELECT * FROM b722 WHERE active AND score = 30;
:e SELECT * FROM b722 WHERE NOT active AND score = 31;
:e SELECT * FROM b722 WHERE active AND score > 290 ORDER BY score DESC LIMIT 3;
:e SELECT * FROM b722 WHERE active;
:e UPDATE b722 SET score = 0 WHERE active AND score = 30;
SELECT id, score FROM b722 WHERE active AND score > 290 ORDER BY score DESC LIMIT 3;
