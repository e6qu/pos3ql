-- An index's pg_class.relpages and reltuples: CREATE INDEX and REINDEX count
-- the btree its build lays out (a metapage, leaves filled to the fillfactor,
-- pivots truncated to the first differing key column, equal keys shared in
-- posting lists), ANALYZE reads the index's pages and the table's rows it
-- holds, and VACUUM recounts entries only after removing dead rows, keeping
-- every page of the file.
CREATE TABLE ic(a int, b text);
CREATE INDEX ic_empty ON ic(a);
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ic%' ORDER BY relname;
INSERT INTO ic SELECT g, md5(g::text) FROM generate_series(1, 6000) g;
CREATE INDEX ic_a ON ic(a);
CREATE INDEX ic_b ON ic(b);
CREATE INDEX ic_half ON ic(a) WHERE a < 4000;
CREATE INDEX ic_ab ON ic(a, b);
CREATE INDEX ic_inc ON ic(a) INCLUDE (b);
CREATE INDEX ic_desc ON ic(a DESC) WITH (fillfactor = 50);
CREATE INDEX ic_expr ON ic((a % 7));
CREATE INDEX ic_nodedup ON ic((a % 7)) WITH (deduplicate_items = off);
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ic%' ORDER BY relname;
ANALYZE ic;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ic%' ORDER BY relname;
-- A partial index's planned rows are its counted tuples.
EXPLAIN SELECT a FROM ic WHERE a < 1000;
-- VACUUM without dead rows leaves the indexes' counts; after a DELETE it
-- counts the entries left in files that keep their pages.
VACUUM ic;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname IN ('ic', 'ic_a', 'ic_b') ORDER BY relname;
DELETE FROM ic WHERE a > 3000;
VACUUM ic;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname IN ('ic_a', 'ic_b', 'ic_half') ORDER BY relname;
REINDEX INDEX ic_b;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname IN ('ic', 'ic_a', 'ic_b') ORDER BY relname;
VACUUM FULL ic;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname IN ('ic', 'ic_a', 'ic_b', 'ic_half') ORDER BY relname;
DROP TABLE ic;
-- Keys with NULLs, duplicates and text of many lengths (the table itself is
-- left out: its rows of varied width refill pages through the free space map).
CREATE TABLE id(k int, t text);
INSERT INTO id SELECT CASE WHEN g % 10 = 0 THEN NULL ELSE g % 100 END,
  repeat('x', g % 200) FROM generate_series(1, 1000) g;
INSERT INTO id SELECT CASE WHEN g % 10 = 0 THEN NULL ELSE g % 100 END,
  repeat('x', g % 200) FROM generate_series(1001, 2000) g;
INSERT INTO id SELECT CASE WHEN g % 10 = 0 THEN NULL ELSE g % 100 END,
  repeat('x', g % 200) FROM generate_series(2001, 3000) g;
INSERT INTO id SELECT CASE WHEN g % 10 = 0 THEN NULL ELSE g % 100 END,
  repeat('x', g % 200) FROM generate_series(3001, 4000) g;
INSERT INTO id SELECT CASE WHEN g % 10 = 0 THEN NULL ELSE g % 100 END,
  repeat('x', g % 200) FROM generate_series(4001, 5000) g;
INSERT INTO id SELECT CASE WHEN g % 10 = 0 THEN NULL ELSE g % 100 END,
  repeat('x', g % 200) FROM generate_series(5001, 6000) g;
INSERT INTO id SELECT CASE WHEN g % 10 = 0 THEN NULL ELSE g % 100 END,
  repeat('x', g % 200) FROM generate_series(6001, 7000) g;
CREATE INDEX id_k ON id(k);
CREATE INDEX id_t ON id(t);
CREATE INDEX id_kt ON id(k, t);
CREATE INDEX id_tk ON id(t, k);
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'id\_%' ORDER BY relname;
DROP TABLE id;
-- Constraint indexes, and a table whose autovacuum is off, whose own counts
-- CREATE INDEX leaves.
CREATE TABLE ip(a int PRIMARY KEY, b int UNIQUE) WITH (autovacuum_enabled = false);
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ip%' ORDER BY relname;
INSERT INTO ip SELECT g, 2 * g FROM generate_series(1, 2000) g;
CREATE INDEX ip_ab ON ip(a, b);
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ip%' ORDER BY relname;
ANALYZE ip;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ip%' ORDER BY relname;
-- TRUNCATE gives the table and its indexes new files nothing has counted,
-- which its own transaction sees at once and a rollback discards.
BEGIN;
TRUNCATE ip;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ip%' ORDER BY relname;
ROLLBACK;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ip%' ORDER BY relname;
BEGIN;
SAVEPOINT s;
TRUNCATE ip;
ROLLBACK TO s;
COMMIT;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ip%' ORDER BY relname;
TRUNCATE ip;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ip%' ORDER BY relname;
-- Counts written after the TRUNCATE stand.
CREATE TABLE iv(a int PRIMARY KEY);
INSERT INTO iv SELECT generate_series(1, 1000);
ANALYZE iv;
BEGIN;
TRUNCATE iv;
INSERT INTO iv SELECT generate_series(1, 10);
REINDEX TABLE iv;
COMMIT;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'iv%' ORDER BY relname;
-- A table created, or already truncated, in the same subtransaction is
-- truncated in place: its indexes are rebuilt empty, and their builds count
-- the table.
BEGIN;
CREATE TABLE iu(a int PRIMARY KEY);
INSERT INTO iu SELECT generate_series(1, 1000);
CREATE INDEX iu_x ON iu(a);
TRUNCATE iu;
COMMIT;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'iu%' ORDER BY relname;
INSERT INTO iv SELECT generate_series(11, 20);
BEGIN;
TRUNCATE iv;
TRUNCATE iv;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'iv%' ORDER BY relname;
COMMIT;
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'iv%' ORDER BY relname;
-- A rolled-back CREATE INDEX leaves the table's counts it made.
CREATE TABLE ir(a int);
INSERT INTO ir SELECT generate_series(1, 1000);
BEGIN;
INSERT INTO ir SELECT generate_series(1, 1000);
CREATE INDEX ir_a ON ir(a);
ROLLBACK;
SELECT relname, relpages, reltuples FROM pg_class WHERE relname LIKE 'ir%' ORDER BY relname;
-- A partitioned index has no storage; its partitions' indexes do.
CREATE TABLE iq(a int) PARTITION BY RANGE (a);
CREATE TABLE iq1 PARTITION OF iq FOR VALUES FROM (0) TO (1000);
CREATE INDEX iq_a ON iq(a);
INSERT INTO iq SELECT generate_series(0, 999);
ANALYZE iq;
SELECT relname, relkind, relpages, reltuples FROM pg_class
WHERE relname LIKE 'iq%' ORDER BY relname;
-- Other access methods' empty indexes.
CREATE TABLE ix(a int, t tsvector, r int4range, p point);
CREATE INDEX ix_hash ON ix USING hash(a);
CREATE INDEX ix_gin ON ix USING gin(t);
CREATE INDEX ix_gist ON ix USING gist(r);
CREATE INDEX ix_spgist ON ix USING spgist(p);
CREATE INDEX ix_brin ON ix USING brin(a);
SELECT relname, relpages, reltuples FROM pg_class
WHERE relname LIKE 'ix%' ORDER BY relname;
DROP TABLE ip, iu, iv, ir, iq, ix;
