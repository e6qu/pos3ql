-- A whole-row reference or ROW(...) over a stored row keeps a compressed
-- value compressed and fetches one saved out of line back in line
-- (toast_build_flattened_tuple), so pg_column_size of the composite counts
-- the stored forms; ROW(...) holding an out-of-line value decompresses every
-- field (toast_flatten_tuple_to_datum). A field of a whole row is the
-- relation's column itself. A record field longer than 8 kB renders whole.
-- ANALYZE's avg_width counts each value as stored: a compressed value's
-- compressed size and an out-of-line value's 18-byte TOAST pointer.
CREATE TABLE wr (id int, a text, b text, c text);
INSERT INTO wr VALUES (1, repeat('a', 3000), 'short', NULL);
INSERT INTO wr VALUES (2, repeat('ab', 5000), repeat('x', 100), 'c');
INSERT INTO wr VALUES (3, (SELECT string_agg(md5(i::text), '') FROM generate_series(1, 300) i), 'q', 'r');
INSERT INTO wr VALUES (4, 'tiny', 'b', 'c');
SELECT id, pg_column_size(a), pg_column_size(wr), pg_column_size(wr.*), pg_column_size(ROW(a, b)), pg_column_size(ROW(id, a, b, c)), pg_column_size((wr.*)::wr), pg_column_size(ROW(a)) FROM wr ORDER BY id;
SELECT id, pg_column_size(x) FROM (SELECT id, wr AS x FROM wr) s ORDER BY id;
SELECT id, pg_column_size(ROW(a, 'lit')), pg_column_size(ROW(a || '')), pg_column_size(ROW(ROW(a), 1)) FROM wr ORDER BY id;
SELECT id, pg_column_size(t), pg_column_size(t.*) FROM wr t ORDER BY id;
SELECT id, pg_column_size((wr).a), pg_column_size((ROW(a, b)).f1), pg_column_size((wr).b) FROM wr ORDER BY id;
SELECT id, pg_column_size(wr) FROM wr WHERE id = 1 UNION ALL SELECT id, pg_column_size(wr) FROM wr WHERE id = 2;
SELECT id, length(wr::text), md5(wr::text) = md5(ROW(wr.*)::text) FROM wr ORDER BY id;
SELECT length(ROW(repeat('x', 9000), repeat('y', 10))::text), length(ROW(repeat('x', 9000))::text);
SELECT (ROW(repeat('x', 9000) || ' z', 1)::text) LIKE '("x%x z",1)';
ANALYZE wr;
SELECT attname, avg_width FROM pg_stats WHERE tablename = 'wr' ORDER BY attname;
CREATE TABLE wr3 AS SELECT id, wr AS x FROM wr WHERE id = 4;
SELECT id, pg_column_size(x), pg_column_size((x).a), length((x).a) FROM wr3 ORDER BY id;
DROP TABLE wr3;
DROP TABLE wr;
CREATE TABLE wide (id int, t text, j json, k text);
ALTER TABLE wide ALTER COLUMN t SET STORAGE EXTERNAL;
INSERT INTO wide VALUES (1, repeat('t', 3000), NULL, repeat('k', 900)), (2, NULL, (SELECT json_agg(i) FROM generate_series(1, 2000) i), (SELECT string_agg(i::text, ',') FROM generate_series(1, 2000) i)), (3, 'u', '1', 'v');
SELECT id, pg_column_size(t), pg_column_size(j), pg_column_size(k), pg_column_size(wide), pg_column_size(ROW(t, j)), pg_column_size(ROW(j)), pg_column_size(ROW(k, j)), pg_column_size(ROW(wide.*)) FROM wide ORDER BY id;
ANALYZE wide;
SELECT attname, avg_width FROM pg_stats WHERE tablename = 'wide' ORDER BY attname;
DROP TABLE wide;
