-- Every unique index build sorts its keys (O(n log n)) and names the first
-- equal pair PostgreSQL's tuplesort compares: ALTER TABLE ADD UNIQUE and
-- PRIMARY KEY before the rows' other checks (phase 2), a rewrite's rebuild
-- of every index in OID order over the copied rows, a partition's cloned
-- keys and ATTACH PARTITION. ALTER COLUMN TYPE creates again the
-- constraint indexes, then the other indexes, that read a retyped column,
-- which so build after the rest. ALTER TABLE's row checks report a NULL as
-- "contains null values" and a CHECK as "violated by some row".
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/tablecmds.c
CREATE TABLE u (id integer, a integer, b text);
INSERT INTO u SELECT g, (g * 37) % 101, 'v' || ((g * 13) % 50) FROM generate_series(1, 300) g;
ALTER TABLE u ADD CONSTRAINT u_a_key UNIQUE (a);
ALTER TABLE u ADD PRIMARY KEY (b);
ALTER TABLE u ADD CONSTRAINT u_c CHECK (id < 0), ADD CONSTRAINT u_a2 UNIQUE (a);
ALTER TABLE u ADD CONSTRAINT u_ab UNIQUE (a, b), ADD COLUMN c integer DEFAULT random();
CREATE TABLE v (a integer UNIQUE, b integer);
INSERT INTO v SELECT g, g FROM generate_series(1, 100) g;
ALTER TABLE v ALTER a TYPE integer USING a % 13;
ALTER TABLE v ALTER a TYPE bigint USING a % 17;
CREATE TABLE w (a integer, b integer);
INSERT INTO w SELECT g, g FROM generate_series(1, 60) g;
CREATE UNIQUE INDEX w_a ON w (a);
CREATE UNIQUE INDEX w_b ON w ((b * 2));
ALTER TABLE w ALTER a TYPE bigint USING a % 11;
ALTER TABLE w ALTER b TYPE bigint USING b % 13;
UPDATE w SET b = NULL WHERE a > 40;
ALTER TABLE w ADD CONSTRAINT w_nnd UNIQUE NULLS NOT DISTINCT (b);
ALTER TABLE w ADD CONSTRAINT w_d UNIQUE (b) DEFERRABLE;
ALTER TABLE w ADD PRIMARY KEY (b);
CREATE TABLE x (a integer, b integer, CONSTRAINT x_pos CHECK (a > 0));
INSERT INTO x VALUES (1, NULL), (2, 2);
ALTER TABLE x ALTER b SET NOT NULL;
ALTER TABLE x ADD COLUMN c integer NOT NULL;
ALTER TABLE x ADD COLUMN c integer NOT NULL DEFAULT random()::int * 0 + NULL;
ALTER TABLE x ADD COLUMN c integer CHECK (c > 0) DEFAULT 0;
ALTER TABLE x ADD COLUMN c integer CHECK (c > 0) DEFAULT random()::int * 0;
ALTER TABLE x ALTER a TYPE bigint USING -a;
ALTER TABLE x ADD CONSTRAINT x_b CHECK (b > 5), ALTER b SET NOT NULL;
ALTER TABLE x ADD CONSTRAINT x_b CHECK (b > 5) NOT VALID;
ALTER TABLE x VALIDATE CONSTRAINT x_b;
ALTER TABLE x ADD PRIMARY KEY (b);
CREATE TABLE p (a integer, b integer) PARTITION BY RANGE (a);
CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (100);
CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (100) TO (200);
INSERT INTO p SELECT g, g % 7 FROM generate_series(0, 199) g;
INSERT INTO p VALUES (150, 3), (150, 3), (20, 6), (20, 6);
ALTER TABLE p ADD CONSTRAINT p_ab UNIQUE (a, b);
ALTER TABLE p ADD PRIMARY KEY (a, b);
CREATE UNIQUE INDEX p_ab_idx ON p (a, b);
DELETE FROM p WHERE a IN (150, 20);
ALTER TABLE p ADD CONSTRAINT p_ab UNIQUE (a, b);
CREATE TABLE p3 (a integer, b integer);
INSERT INTO p3 SELECT 200 + g % 30, g % 3 FROM generate_series(1, 90) g;
ALTER TABLE p ATTACH PARTITION p3 FOR VALUES FROM (200) TO (300);
CREATE TABLE r (a integer, b integer, c integer);
INSERT INTO r SELECT g, g % 7, g % 9 FROM generate_series(1, 100) g;
CREATE UNIQUE INDEX r_a ON r (a);
ALTER TABLE r ADD CONSTRAINT r_b UNIQUE (b), ADD CONSTRAINT r_c UNIQUE (c);
ALTER TABLE r ADD CONSTRAINT r_c UNIQUE (c), ADD CONSTRAINT r_b UNIQUE (b);
UPDATE r SET a = a % 9;
REINDEX TABLE r;
CREATE TABLE y (a integer, b text, c integer, d text);
INSERT INTO y VALUES (1, '1', 5, '1'), (2, '01', 6, '2');
CREATE UNIQUE INDEX y_bp ON y (b) WHERE a > 0;
ALTER TABLE y ADD CONSTRAINT y_u UNIQUE (b);
CREATE UNIQUE INDEX y_c ON y (c);
CREATE UNIQUE INDEX y_d ON y ((d::integer));
ALTER TABLE y ALTER b TYPE integer USING b::integer;
ALTER TABLE y ALTER d TYPE integer USING d::integer;
ALTER TABLE y ADD CONSTRAINT y_dk UNIQUE (d);
ALTER TABLE y ALTER d TYPE bigint USING d::bigint * 0;
DROP INDEX y_bp;
ALTER TABLE y DROP CONSTRAINT y_u;
CREATE UNIQUE INDEX y_b2 ON y (b);
ALTER TABLE y ALTER b TYPE integer USING b::integer, ALTER d TYPE integer USING d::integer;
DROP TABLE u, v, w, x, p, p3, r, y;
