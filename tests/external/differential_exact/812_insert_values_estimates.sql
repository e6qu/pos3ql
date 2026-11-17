-- An INSERT of listed rows reads a Result or Values Scan whose width is
-- each attribute's type width (a dropped one counts as an integer NULL),
-- and whose cost is cpu_tuple_cost plus its values' evaluation. A write
-- with RETURNING returns its input's rows at its width. Unknown columns in
-- an index or exclusion key expression are positioned in the statement.
CREATE TABLE w812 (a int, b int);
CREATE TABLE v812 (a int, b text, c numeric, d int8, e varchar(10), f char(3),
                   g numeric(10, 2), h bit(20), i boolean, j timestamptz);
EXPLAIN INSERT INTO w812 VALUES (1, 2);
EXPLAIN INSERT INTO w812 VALUES (1 + 1, 2 * 3);
EXPLAIN INSERT INTO w812 VALUES (1, 2), (3, 4);
EXPLAIN INSERT INTO w812 VALUES (1, 2), (3, 4), (5, 6);
EXPLAIN INSERT INTO v812 VALUES (1, 'abc', 1.5, 7, 'x', 'y', 2.5, B'1', true, now());
EXPLAIN INSERT INTO v812 (a, b) VALUES (1, now()::text);
EXPLAIN INSERT INTO v812 (a) VALUES (length('abc'));
EXPLAIN INSERT INTO v812 DEFAULT VALUES;
EXPLAIN INSERT INTO w812 VALUES (1, 2) RETURNING *;
EXPLAIN INSERT INTO w812 VALUES (1, 2), (3, 4) RETURNING a;
ALTER TABLE w812 ALTER b SET DEFAULT 5;
EXPLAIN INSERT INTO w812 (a) VALUES (1);
EXPLAIN INSERT INTO w812 DEFAULT VALUES;
EXPLAIN INSERT INTO w812 VALUES (1, DEFAULT);
ALTER TABLE v812 DROP COLUMN c, DROP COLUMN j;
EXPLAIN INSERT INTO v812 (a) VALUES (1);
INSERT INTO w812 VALUES (1, 2), (3, 4);
ANALYZE w812;
EXPLAIN INSERT INTO w812 VALUES (1, 2);
EXPLAIN UPDATE w812 SET b = b + 1 RETURNING *;
EXPLAIN DELETE FROM w812 WHERE a = 1 RETURNING a;
CREATE INDEX ON w812 ((a + z));
CREATE INDEX ON w812 (lower(zz));
ALTER TABLE w812 ADD EXCLUDE USING btree ((a * q) WITH =);
ALTER TABLE w812 ADD EXCLUDE USING btree (a WITH =) WHERE (q > 1);
ALTER TABLE w812 ADD EXCLUDE USING btree (lower(qq) WITH =);
ALTER TABLE w812 ADD EXCLUDE USING btree ((w812.q) WITH =);
ALTER TABLE w812 ADD EXCLUDE USING btree (q WITH =);
CREATE TABLE x812 (a int, EXCLUDE USING btree ((a + z) WITH =));
CREATE TABLE x812 (a int, EXCLUDE USING btree (a WITH =) WHERE (z > 0));
DROP TABLE w812, v812;
