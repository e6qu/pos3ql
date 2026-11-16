-- A btree of several key columns: conditions on its leading columns bound
-- the scan, later columns' conditions only filter index entries, and a
-- column without a condition is bridged by a skip array when its few
-- distinct values make that cheaper (btcostestimate). Its key order serves
-- an ORDER BY of several keys, in either direction when every key flips,
-- and a leading part of it an incremental sort. The rows stay under the
-- forced-spill run's 256 KiB memtable.
CREATE TABLE m720 (id int PRIMARY KEY, a int, b int, c int, d int, t text);
INSERT INTO m720
  SELECT g, g % 20, (g * 7) % 50, g % 7, CASE WHEN g % 11 = 0 THEN NULL ELSE g % 13 END,
         'v' || (g % 100)
  FROM generate_series(1, 3000) g;
CREATE INDEX m720_ab ON m720 (a, b);
CREATE INDEX m720_cd ON m720 (c DESC, d NULLS FIRST);
ANALYZE m720;
\set e 'EXPLAIN (COSTS OFF)'
:e SELECT * FROM m720 WHERE a = 3 AND b = 4;
:e SELECT * FROM m720 WHERE a = 3 AND b > 40;
:e SELECT * FROM m720 WHERE a < 2 AND b = 4;
:e SELECT * FROM m720 WHERE a IN (1, 2) AND b = 4;
:e SELECT * FROM m720 WHERE b = 4;
:e SELECT * FROM m720 WHERE b BETWEEN 4 AND 6;
:e SELECT * FROM m720 WHERE c = 2 AND d IS NULL;
:e SELECT * FROM m720 WHERE d = 5;
:e SELECT a, b FROM m720 WHERE a = 3;
:e SELECT a, b FROM m720 WHERE b = 4;
:e SELECT * FROM m720 WHERE a = 3 ORDER BY b;
:e SELECT * FROM m720 WHERE a = 3 ORDER BY b DESC LIMIT 3;
:e SELECT * FROM m720 ORDER BY a, b LIMIT 5;
:e SELECT * FROM m720 ORDER BY a DESC, b DESC LIMIT 5;
:e SELECT * FROM m720 ORDER BY a, b DESC LIMIT 5;
:e SELECT * FROM m720 ORDER BY a, id LIMIT 5;
:e SELECT * FROM m720 ORDER BY c DESC, d NULLS FIRST LIMIT 5;
:e SELECT * FROM m720 ORDER BY c, d DESC LIMIT 5;
:e SELECT * FROM m720 ORDER BY c, d LIMIT 5;
:e SELECT * FROM m720 WHERE c = 4 ORDER BY d DESC NULLS LAST LIMIT 4;
:e SELECT * FROM m720 WHERE a = 2 AND b = 9 ORDER BY a, b, id;
:e SELECT * FROM m720 ORDER BY a, b;
SET enable_incremental_sort = off;
:e SELECT * FROM m720 ORDER BY a, id LIMIT 5;
RESET enable_incremental_sort;
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM m720 WHERE a = 3 AND b > 40;
:a SELECT * FROM m720 WHERE a = 3 AND b > 40 AND t = 'v3';
:a SELECT * FROM m720 ORDER BY a, b LIMIT 5;
-- Rows come back in the chosen path's order.
SELECT id, a, b FROM m720 WHERE a = 3 AND b > 40;
SELECT id, a, b FROM m720 WHERE a = 3 ORDER BY b DESC LIMIT 3;
SELECT id, a, b FROM m720 ORDER BY a DESC, b DESC LIMIT 5;
SELECT id, a, b FROM m720 ORDER BY a, id LIMIT 5;
SELECT id, c, d FROM m720 ORDER BY c DESC, d NULLS FIRST LIMIT 5;
SELECT id, c, d FROM m720 WHERE c = 4 ORDER BY d DESC NULLS LAST LIMIT 4;
SELECT id, a, b FROM m720 WHERE b = 4 LIMIT 6;
