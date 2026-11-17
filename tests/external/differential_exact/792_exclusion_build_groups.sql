-- An exclusion constraint's build compares the rows of other operators
-- only among the rows its equality elements make equal, and names the first
-- conflicting row in heap order as PostgreSQL's index build does.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/index.c
CREATE TABLE t792 (room int4range, slot int4range, note text);
INSERT INTO t792 SELECT int4range(g % 40, g % 40 + 1), int4range(g, g + 1), 'r' || g FROM generate_series(1, 1200) g;
ALTER TABLE t792 ADD CONSTRAINT t792_adjacent EXCLUDE USING gist (room WITH =, slot WITH -|-);
INSERT INTO t792 VALUES (int4range(7, 8), int4range(5000, 5002), 'late');
INSERT INTO t792 VALUES (int4range(7, 8), int4range(5002, 5003), 'touching');
SELECT conname FROM pg_constraint WHERE conrelid = 't792'::regclass ORDER BY 1;
ALTER TABLE t792 DROP CONSTRAINT t792_adjacent;
INSERT INTO t792 VALUES (int4range(9, 10), int4range(6000, 6002), 'first'), (int4range(9, 10), int4range(6002, 6004), 'second');
ALTER TABLE t792 ADD CONSTRAINT t792_adjacent EXCLUDE USING gist (room WITH =, slot WITH -|-);
DELETE FROM t792 WHERE note = 'second';
ALTER TABLE t792 ADD CONSTRAINT t792_adjacent EXCLUDE USING gist (room WITH =, slot WITH -|-);
SELECT count(*) FROM t792;
