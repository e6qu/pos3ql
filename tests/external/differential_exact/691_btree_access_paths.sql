-- A query reading one table chooses among its sequential scan, each btree
-- index's index or index-only scan and a bitmap heap scan by PostgreSQL's
-- costs: cost_index with btcostestimate and the column's correlation,
-- cost_bitmap_heap_scan and cost_seqscan, with selectivities from the
-- statistics ANALYZE gathers (most common values, histogram bounds,
-- null fraction) and the defaults of a table never analyzed. The scan
-- returns its rows in the chosen path's order: an index scan by key, a
-- bitmap heap scan and a sequential scan by heap position. The rows stay
-- under the forced-spill run's 256 KiB memtable.
CREATE TABLE p691 (id int PRIMARY KEY, k int, t text, f float8, d date, n int);
INSERT INTO p691
  SELECT g, g % 300, 'k' || (g % 1000), g / 7.0, date '2021-01-01' + g % 500,
         CASE WHEN g % 3 = 0 THEN NULL ELSE g END
  FROM generate_series(1, 3000) g;
CREATE INDEX p691_k ON p691 (k);
CREATE INDEX p691_f ON p691 (f);
CREATE INDEX p691_d ON p691 (d DESC);
CREATE INDEX p691_n ON p691 (n);
\set e 'EXPLAIN (COSTS OFF)'
-- Never analyzed: CREATE INDEX counted the rows, the conditions take
-- PostgreSQL's default selectivities.
:e SELECT * FROM p691 WHERE k = 17;
:e SELECT * FROM p691 WHERE k < 5;
:e SELECT * FROM p691 WHERE n IS NULL;
:e SELECT * FROM p691 WHERE k <> 5;
ANALYZE p691;
:e SELECT * FROM p691 WHERE k = 17;
:e SELECT * FROM p691 WHERE k < 5;
:e SELECT * FROM p691 WHERE k > 290;
:e SELECT * FROM p691 WHERE k BETWEEN 10 AND 12;
:e SELECT * FROM p691 WHERE k BETWEEN 10 AND 120;
:e SELECT * FROM p691 WHERE f < 3;
:e SELECT * FROM p691 WHERE f > 100;
:e SELECT * FROM p691 WHERE d = '2021-02-01';
:e SELECT * FROM p691 WHERE d < '2021-01-05';
:e SELECT * FROM p691 WHERE n IS NULL;
:e SELECT * FROM p691 WHERE n IS NOT NULL;
:e SELECT * FROM p691 WHERE n < 100;
:e SELECT * FROM p691 WHERE k <> 5;
:e SELECT * FROM p691 WHERE k = 17 AND f < 100;
:e SELECT * FROM p691 WHERE id > 2990;
:e SELECT * FROM p691 WHERE id > 5000;
:e SELECT * FROM p691 WHERE 5 > k;
:e SELECT k FROM p691 WHERE k < 3;
:e SELECT count(*) FROM p691;
SET enable_bitmapscan = off;
:e SELECT * FROM p691 WHERE k = 17;
SET enable_indexscan = off;
:e SELECT * FROM p691 WHERE k = 17;
SET enable_seqscan = off;
:e SELECT * FROM p691 WHERE k < 200;
RESET enable_bitmapscan;
RESET enable_indexscan;
RESET enable_seqscan;
-- What each chosen path reads: the rows its index conditions reach, the
-- rows its filter removes, the heap pages a bitmap heap scan visits and
-- the heap fetches of an index-only scan, none once VACUUM has set the
-- visibility map.
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM p691 WHERE id = 5;
:a SELECT * FROM p691 WHERE id = 5 AND t = 'zz';
:a SELECT * FROM p691 WHERE k = 17 AND t = 'k17';
:a SELECT * FROM p691 WHERE id < 40 AND k > 20;
:a SELECT * FROM p691 WHERE k < 3 AND id < 2000;
:a SELECT * FROM p691 WHERE f > 420;
:a SELECT id FROM p691 WHERE id < 30;
:a SELECT * FROM p691 WHERE f < 0;
VACUUM p691;
:a SELECT id FROM p691 WHERE id < 30;
-- Rows come back in the chosen path's order.
SELECT id, k FROM p691 WHERE k = 7 AND id > 2000;
SELECT id, f FROM p691 WHERE f > 427;
SELECT id, d FROM p691 WHERE d > '2022-05-12' AND id > 2900;
SELECT id, n FROM p691 WHERE n < 12;
UPDATE p691 SET t = 'moved' WHERE id IN (4, 8);
SELECT id, t FROM p691 WHERE id < 10;
SELECT id, t FROM p691 WHERE t = 'moved' OR id < 3;
