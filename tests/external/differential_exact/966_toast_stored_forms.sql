-- A stored row longer than TOAST's 2032-byte threshold keeps its widest
-- values compressed with pglz or saved out of line, and pg_column_size of a
-- stored column reports that form. An UPDATE keeps the form of a value it
-- does not change and forms the new row's other values anew. The heap pages
-- ANALYZE counts hold the values in their stored forms.
CREATE TABLE ts (id int, a text, b text, c bytea);
INSERT INTO ts VALUES (1, repeat('y', 1500), repeat('y', 1500), NULL);
INSERT INTO ts VALUES (2, repeat('x', 3000), NULL, NULL);
INSERT INTO ts VALUES (3, repeat('abcdefghij', 300), 'short', NULL);
INSERT INTO ts VALUES (4, repeat('y', 100), repeat('w', 100), NULL);
INSERT INTO ts SELECT 5, string_agg(md5(i::text), ''), NULL, NULL FROM generate_series(1, 300) i;
INSERT INTO ts SELECT 6, NULL, NULL, decode(string_agg(md5(i::text), ''), 'hex') FROM generate_series(1, 200) i;
INSERT INTO ts VALUES (7, repeat('é', 1500), repeat('z', 40), '\x00');
SELECT id, pg_column_size(a), pg_column_size(b), pg_column_size(c) FROM ts ORDER BY id;
UPDATE ts SET b = 'z' WHERE id = 1;
UPDATE ts SET id = 12 WHERE id = 2;
UPDATE ts SET b = repeat('v', 1900) WHERE id = 4;
SELECT id, pg_column_size(a), pg_column_size(b), pg_column_size(c) FROM ts ORDER BY id;
ALTER TABLE ts ALTER COLUMN b SET STORAGE EXTERNAL;
INSERT INTO ts VALUES (8, repeat('k', 1200), repeat('k', 1200), NULL);
ALTER TABLE ts ALTER COLUMN a SET STORAGE MAIN;
INSERT INTO ts VALUES (9, repeat('m', 3000), repeat('n', 30), NULL);
SELECT id, pg_column_size(a), pg_column_size(b), pg_column_size(c) FROM ts ORDER BY id;
CREATE TABLE tp (id int, v text);
INSERT INTO tp SELECT i, repeat(chr(65 + i % 26), 2100 + i % 50) FROM generate_series(1, 400) i;
VACUUM ANALYZE tp;
SELECT relpages, reltuples FROM pg_class WHERE relname = 'tp';
SELECT max(pg_column_size(v)), min(pg_column_size(v)) FROM tp;
DROP TABLE ts, tp;
