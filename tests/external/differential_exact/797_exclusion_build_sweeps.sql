-- An exclusion constraint without equality elements is checked over its
-- existing rows as building its index does (`IndexCheckExclusion`): the
-- error names the first row in heap order that conflicts with another, and
-- a row it conflicts with. Rows are swept in lower-bound order of a range
-- overlap element, so only overlapping rows are compared on the others.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/index.c
CREATE TABLE t797 (r int4range, s int4range, b box);
INSERT INTO t797 SELECT int4range(g, g + 40), int4range(g * 3, g * 3 + 2), box(point(g, 0), point(g + 0.5, 1)) FROM generate_series(1, 3000) g;
ALTER TABLE t797 ADD CONSTRAINT t797_rs EXCLUDE USING gist (r WITH &&, s WITH &&);
ALTER TABLE t797 ADD CONSTRAINT t797_adjacent EXCLUDE USING gist (r WITH &&, s WITH -|-);
ALTER TABLE t797 ADD CONSTRAINT t797_b EXCLUDE USING gist (b WITH &&);
ALTER TABLE t797 DROP CONSTRAINT t797_rs;
ALTER TABLE t797 DROP CONSTRAINT t797_b;
INSERT INTO t797 VALUES (int4range(2990, 2995), int4range(8970, 8975), box(point(2999.9, 0.5), point(3000.2, 2)));
INSERT INTO t797 VALUES (NULL, int4range(1, 2), NULL), (int4range(5, 5), int4range(1, 2), NULL);
ALTER TABLE t797 ADD CONSTRAINT t797_rs EXCLUDE USING gist (r WITH &&, s WITH &&);
ALTER TABLE t797 ADD CONSTRAINT t797_b EXCLUDE USING gist (b WITH &&);
ALTER TABLE t797 ADD CONSTRAINT t797_adjacent2 EXCLUDE USING gist (r WITH &&, s WITH -|-);
DELETE FROM t797 WHERE r = int4range(2990, 2995);
ALTER TABLE t797 ADD CONSTRAINT t797_rs EXCLUDE USING gist (r WITH &&, s WITH &&);
ALTER TABLE t797 ADD CONSTRAINT t797_b EXCLUDE USING gist (b WITH &&);
SELECT conname FROM pg_constraint WHERE conrelid = 't797'::regclass ORDER BY conname;
CREATE TABLE u797 (r int4range, s int4range);
INSERT INTO u797 SELECT int4range(g, g + 3), int4range(g % 17, g % 17 + 1) FROM generate_series(1, 2000) g;
ALTER TABLE u797 ADD CONSTRAINT u797_adjacent EXCLUDE USING gist (r WITH &&, s WITH -|-);
SELECT box '((3,1),(2,0))' && box '((2.5,2),(1.5,0.5))', box '((3,1),(2,0))' && box '((1.9,2),(1.5,0.5))', box '((1,1),(0,0))' && box '((2,2),(1,1))';
