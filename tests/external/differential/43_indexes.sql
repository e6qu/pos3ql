-- Explicit indexes: the pg_indexes view, pg_get_indexdef shapes, and — the
-- part a padding bug once silently broke — UNIQUE index enforcement over
-- exactly the written columns, no more and no fewer.
DROP TABLE IF EXISTS ix;
CREATE TABLE ix (a int PRIMARY KEY, b text, c int);
CREATE INDEX ix_bc ON ix (b, c);
CREATE UNIQUE INDEX ix_b ON ix (b);
SELECT schemaname, tablename, indexname, indexdef FROM pg_indexes WHERE tablename = 'ix' ORDER BY indexname;

-- A UNIQUE index on (b) alone: a duplicate b must collide even when every
-- other column differs.
INSERT INTO ix VALUES (1, 'x', 10);
INSERT INTO ix VALUES (2, 'x', 20);
INSERT INTO ix VALUES (3, 'y', 10);
SELECT a, b, c FROM ix ORDER BY a;

-- Multi-column UNIQUE: the pair must repeat to collide.
DROP TABLE IF EXISTS ix2;
CREATE TABLE ix2 (p int, q int);
CREATE UNIQUE INDEX ix2_pq ON ix2 (p, q);
INSERT INTO ix2 VALUES (1, 1), (1, 2), (2, 1);
INSERT INTO ix2 VALUES (1, 2);
SELECT count(*) FROM ix2;

DROP TABLE ix;
DROP TABLE ix2;
