-- An UPDATE or ON CONFLICT DO UPDATE keeps the stored datums its new row
-- passes on: an unchanged value keeps its form in place, a value moved to
-- another column is fetched back in line and stored anew there. A table
-- rewrite and INSERT ... SELECT from the same table do the same.
CREATE TABLE kv (k int PRIMARY KEY, a text, b text, n int);
INSERT INTO kv VALUES (1, repeat('y', 1500), repeat('w', 1500), 0);
INSERT INTO kv SELECT 2, string_agg(md5(i::text), ''), 'b', 0 FROM generate_series(1, 120) i;
SELECT k, pg_column_size(a), pg_column_size(b) FROM kv ORDER BY k;
UPDATE kv SET n = n + 1;
SELECT k, pg_column_size(a), pg_column_size(b) FROM kv ORDER BY k;
INSERT INTO kv VALUES (1, 'x', repeat('v', 100), 5)
  ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n, b = kv.a;
SELECT k, pg_column_size(a), pg_column_size(b), n FROM kv ORDER BY k;
UPDATE kv SET a = b, b = a WHERE k = 2;
SELECT k, pg_column_size(a), pg_column_size(b) FROM kv ORDER BY k;
ALTER TABLE kv ADD COLUMN c text DEFAULT 'c';
ALTER TABLE kv ALTER COLUMN n TYPE bigint;
SELECT k, pg_column_size(a), pg_column_size(b), pg_column_size(c) FROM kv ORDER BY k;
DELETE FROM kv WHERE k = 1;
INSERT INTO kv SELECT k + 10, b, a, n FROM kv;
SELECT k, pg_column_size(a), pg_column_size(b) FROM kv ORDER BY k;
DROP TABLE kv;
