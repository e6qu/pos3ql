-- A row that breaks several unique or exclusion constraints names the one
-- whose index PostgreSQL created first (checks run in index OID order).
CREATE TABLE uo_keys (a int, b int, UNIQUE (a, b), c int UNIQUE, d int PRIMARY KEY);
INSERT INTO uo_keys VALUES (1, 1, 1, 1);
INSERT INTO uo_keys VALUES (1, 1, 1, 1);
INSERT INTO uo_keys VALUES (2, 2, 1, 1);
INSERT INTO uo_keys VALUES (2, 2, 2, 1);
CREATE UNIQUE INDEX uo_keys_early ON uo_keys (b);
ALTER TABLE uo_keys ADD CONSTRAINT uo_keys_late UNIQUE (a);
INSERT INTO uo_keys VALUES (1, 3, 3, 3);
INSERT INTO uo_keys VALUES (3, 1, 3, 3);
INSERT INTO uo_keys VALUES (1, 1, 3, 3);
UPDATE uo_keys SET a = 1, b = 1 WHERE d = 1;
DROP INDEX uo_keys_early;
CREATE UNIQUE INDEX uo_keys_early ON uo_keys (b);
INSERT INTO uo_keys VALUES (1, 1, 3, 3);
CREATE TABLE uo_index_first (a int, b int);
CREATE UNIQUE INDEX uo_index_first_a ON uo_index_first (a);
ALTER TABLE uo_index_first ADD PRIMARY KEY (b);
INSERT INTO uo_index_first VALUES (1, 1);
INSERT INTO uo_index_first VALUES (1, 1);
CREATE TABLE uo_exclusion (a int, b int, EXCLUDE USING btree (a WITH =), UNIQUE (b));
INSERT INTO uo_exclusion VALUES (1, 1);
INSERT INTO uo_exclusion VALUES (1, 1);
ALTER TABLE uo_exclusion ADD CONSTRAINT uo_exclusion_b_too EXCLUDE USING btree (b WITH =);
INSERT INTO uo_exclusion VALUES (2, 1);
CREATE TABLE uo_parts (a int, b int, UNIQUE (b, a), UNIQUE (a, b)) PARTITION BY RANGE (a);
CREATE TABLE uo_parts_1 PARTITION OF uo_parts FOR VALUES FROM (0) TO (10);
INSERT INTO uo_parts VALUES (1, 1);
INSERT INTO uo_parts VALUES (1, 1);
SELECT c.relname FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
 WHERE i.indrelid IN ('uo_keys'::regclass, 'uo_index_first'::regclass, 'uo_exclusion'::regclass)
 ORDER BY i.indexrelid;
DROP TABLE uo_keys, uo_index_first, uo_exclusion, uo_parts;
