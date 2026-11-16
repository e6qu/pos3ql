-- IN lists and `op ANY (array)` are index conditions (ScalarArrayOpExpr)
-- estimated element by element (scalararraysel); an OR of comparisons of
-- one column with constants becomes one (match_orclause_to_indexcol); an
-- OR over several indexes combines their bitmaps in a BitmapOr, arms on
-- one index column grouped first (group_similar_or_args); and conditions
-- on several indexes combine in a BitmapAnd when that lowers the cost
-- (choose_bitmap_and). The rows stay under the forced-spill run's 256 KiB
-- memtable.
CREATE TABLE s721 (id int PRIMARY KEY, a int, b int, c int, t text);
INSERT INTO s721
  SELECT g, g % 40, g % 97, g % 1000, 'w' || (g % 300) FROM generate_series(1, 4000) g;
CREATE INDEX s721_a ON s721 (a);
CREATE INDEX s721_b ON s721 (b);
CREATE INDEX s721_c ON s721 (c);
CREATE INDEX s721_t ON s721 (t);
ANALYZE s721;
\set e 'EXPLAIN (COSTS OFF)'
:e SELECT * FROM s721 WHERE id IN (5, 9, 1);
:e SELECT * FROM s721 WHERE id IN (5);
:e SELECT * FROM s721 WHERE a IN (1, 2, 3);
:e SELECT * FROM s721 WHERE a IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
:e SELECT * FROM s721 WHERE c = ANY (ARRAY[3, 4, 5]);
:e SELECT * FROM s721 WHERE c = ANY ('{3,4,5}');
:e SELECT * FROM s721 WHERE c NOT IN (3, 4);
:e SELECT * FROM s721 WHERE c < ANY (ARRAY[3, 8]);
:e SELECT * FROM s721 WHERE t IN ('w1', 'w2') AND id < 100;
:e SELECT * FROM s721 WHERE c = 3 OR c = 10;
:e SELECT * FROM s721 WHERE id = 3 OR c = 10;
:e SELECT * FROM s721 WHERE c = 3 OR c = 10 OR b = 7;
:e SELECT * FROM s721 WHERE (id < 3 OR id > 3998) AND a = 1;
:e SELECT * FROM s721 WHERE c = 3 OR (b = 7 AND a = 7);
:e SELECT * FROM s721 WHERE c = 3 OR t = 'w5';
:e SELECT * FROM s721 WHERE c = 3 OR b < 2 OR id > 3990;
:e SELECT * FROM s721 WHERE c = 3 OR a + 1 = 5;
:e SELECT * FROM s721 WHERE a = 3 AND b = 10;
:e SELECT * FROM s721 WHERE a = 3 AND c < 50;
:e SELECT * FROM s721 WHERE a = 3 AND b = 10 AND c < 500;
:e SELECT * FROM s721 WHERE a IN (3, 4) AND b IN (10, 11);
:e SELECT id FROM s721 WHERE id IN (9, 5, 1) ORDER BY id DESC;
:e SELECT * FROM s721 WHERE c IN (3, 4) ORDER BY c LIMIT 5;
SET enable_bitmapscan = off;
:e SELECT * FROM s721 WHERE c = 3 OR c = 10;
:e SELECT * FROM s721 WHERE a IN (1, 2, 3);
RESET enable_bitmapscan;
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM s721 WHERE id = 3 OR c = 10;
:a SELECT * FROM s721 WHERE a = 3 AND b = 10;
-- Rows come back in the chosen path's order.
SELECT id, c FROM s721 WHERE c = 3 OR c = 10;
SELECT id, c FROM s721 WHERE c IN (10, 3) AND id < 2000;
SELECT id FROM s721 WHERE id IN (9, 5, 1);
SELECT id FROM s721 WHERE id IN (9, 5, 1) ORDER BY id DESC;
SELECT id, a, b FROM s721 WHERE a = 3 AND b = 10;
SELECT id, c, b FROM s721 WHERE c = 3 OR b = 7 LIMIT 8;
SET enable_bitmapscan = off;
SELECT id, c FROM s721 WHERE c = 3 OR c = 10;
RESET enable_bitmapscan;
