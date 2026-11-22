-- Stored forms pass through partition scans, MERGE, a row moving to another
-- partition, hash joins, materialized CTEs and UNION ALL; the heap pages
-- VACUUM counts hold the passed values in their stored forms.
CREATE TABLE pt (id int, a text) PARTITION BY RANGE (id);
CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);
CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (10) TO (20);
INSERT INTO pt VALUES (1, repeat('p', 3000)), (2, repeat('q', 1800) || repeat('r', 300));
INSERT INTO pt SELECT 11, string_agg(md5(i::text), '') FROM generate_series(1, 100) i;
SELECT id, pg_column_size(a) FROM pt ORDER BY id, 2;
CREATE TABLE tgt (id int, a text, note text);
INSERT INTO tgt SELECT id, a, 'n' FROM pt;
MERGE INTO tgt t USING pt s ON t.id = s.id + 100
  WHEN NOT MATCHED THEN INSERT VALUES (s.id + 100, s.a, 'm');
SELECT id, pg_column_size(a) FROM tgt ORDER BY id;
UPDATE pt SET id = id + 10 WHERE id = 1;
SELECT id, pg_column_size(a) FROM pt ORDER BY id, 2;
SET enable_nestloop = off;
SET enable_mergejoin = off;
SELECT p.id, pg_column_size(t.a), pg_column_size(p.a) FROM pt p JOIN tgt t ON t.id + 10 = p.id ORDER BY 1, 2, 3;
RESET enable_nestloop;
RESET enable_mergejoin;
INSERT INTO tgt SELECT p.id + 200, p.a, t.note FROM pt p JOIN tgt t ON t.id = p.id;
SELECT id, pg_column_size(a) FROM tgt WHERE id > 200 ORDER BY id, 2;
WITH w AS MATERIALIZED (SELECT id, a FROM pt) SELECT id, pg_column_size(a) FROM w ORDER BY id, 2;
SELECT id, pg_column_size(a) FROM pt UNION ALL SELECT id, pg_column_size(a) FROM tgt WHERE id < 20 ORDER BY 1, 2;
VACUUM ANALYZE tgt;
SELECT relpages FROM pg_class WHERE relname = 'tgt';
DROP TABLE pt, tgt;
