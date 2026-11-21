-- A tsquery has plain storage: TOAST never compresses it or moves it out of
-- line, so a row it keeps wider than a heap page fails as PostgreSQL's
-- heap_insert does, after the row's other values are toasted. Its stored
-- size reads as formed, also through joins and ANALYZE.
CREATE TABLE tq (id int, t text, v tsquery);
INSERT INTO tq SELECT 1, repeat('abc', 1000), string_agg(format('w%s_%s', i, repeat('x', 20)), ' & ')::tsquery FROM generate_series(1, 60) i;
INSERT INTO tq SELECT 2, repeat('q', 30), string_agg(format('w%s_%s', i, repeat('x', 20)), ' & ')::tsquery FROM generate_series(1, 100) i;
INSERT INTO tq SELECT 3, repeat('q', 30), string_agg(format('w%s_%s', i, repeat('x', 20)), ' & ')::tsquery FROM generate_series(1, 170) i;
INSERT INTO tq SELECT 4, NULL, string_agg(format('w%s_%s', i, repeat('x', 20)), ' & ')::tsquery FROM generate_series(1, 170) i;
INSERT INTO tq SELECT 5, repeat('r', 3000), string_agg(format('w%s_%s', i, repeat('x', 20)), ' & ')::tsquery FROM generate_series(1, 110) i;
UPDATE tq SET v = v && v WHERE id = 2;
UPDATE tq SET v = v && 'w1_xxxxxxxxxxxxxxxxxxxx'::tsquery WHERE id = 1;
SELECT id, pg_column_size(t), pg_column_size(v), numnode(v) FROM tq ORDER BY id;
SELECT a.id, pg_column_size(a.v), pg_column_size(b.t) FROM tq a JOIN tq b ON a.id = b.id ORDER BY a.id;
VACUUM ANALYZE tq;
SELECT relpages, reltuples FROM pg_class WHERE relname = 'tq';
SELECT avg_width FROM pg_stats WHERE tablename = 'tq' AND attname = 'v';
INSERT INTO tq SELECT 6, NULL, !!(SELECT v FROM tq WHERE id = 1);
SELECT id, pg_typeof(!!v), numnode(!!v) FROM tq ORDER BY id;
DROP TABLE tq;
CREATE TABLE wide (a text, b text, c text);
ALTER TABLE wide ALTER COLUMN a SET STORAGE PLAIN, ALTER COLUMN b SET STORAGE MAIN;
INSERT INTO wide VALUES (repeat('a', 5000), NULL, NULL);
INSERT INTO wide VALUES (repeat('a', 9000), NULL, NULL);
INSERT INTO wide VALUES (repeat('a', 4000), (SELECT string_agg(md5(i::text), '') FROM generate_series(1, 200) i), NULL);
INSERT INTO wide VALUES (NULL, (SELECT string_agg(md5(i::text), '') FROM generate_series(1, 300) i), repeat('c', 9000));
SELECT pg_column_size(a), pg_column_size(b), pg_column_size(c) FROM wide ORDER BY 1, 2, 3;
DROP TABLE wide;
SELECT OPERATOR(pg_catalog.!!) 'a & b'::tsquery, pg_typeof(OPERATOR(pg_catalog.!!) 'a'::tsquery);
