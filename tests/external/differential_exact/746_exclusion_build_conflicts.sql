-- Building an exclusion constraint's index, for ADD CONSTRAINT, REINDEX or a
-- table rewrite, checks each row in heap order against the others and names
-- the first row that conflicts and a row it conflicts with
-- (`IndexCheckExclusion`); a rewrite reports the same pair as a build.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/index.c
CREATE TABLE g746 (id integer, r int4range, CONSTRAINT g746_x EXCLUDE USING gist (r WITH &&));
INSERT INTO g746 VALUES (1, '[1,5)'), (2, '[10,20)'), (3, '[30,40)'), (4, '[50,60)');
ALTER TABLE g746 ALTER COLUMN r TYPE int8range USING CASE WHEN id = 4 THEN '[12,13)'::int8range WHEN id = 3 THEN '[11,12)'::int8range ELSE r::text::int8range END;
CREATE TABLE h746 (id integer, k integer, CONSTRAINT h746_x EXCLUDE USING btree (k WITH =));
INSERT INTO h746 VALUES (1, 1), (2, 2), (3, 3), (4, 4);
ALTER TABLE h746 ALTER COLUMN k TYPE bigint USING CASE WHEN id = 4 THEN 2 WHEN id = 3 THEN 2 ELSE k END;
CREATE TABLE e746 (id integer, room int4range, r int4range);
INSERT INTO e746 VALUES (1, '[1,1]', '[1,5)'), (2, '[2,2]', '[1,5)'), (3, '[1,1]', '[8,9)'), (4, '[1,1]', '[4,9)'), (5, '[2,2]', 'empty'), (6, '[2,2]', NULL), (7, '[2,2]', '[0,100)');
ALTER TABLE e746 ADD CONSTRAINT e746_x EXCLUDE USING gist (room WITH =, r WITH &&);
ALTER TABLE e746 ADD CONSTRAINT e746_r EXCLUDE USING gist (r WITH &&) WHERE (id > 3);
DELETE FROM e746 WHERE id = 7;
ALTER TABLE e746 ADD CONSTRAINT e746_r EXCLUDE USING gist (r WITH &&) WHERE (id > 3);
ALTER TABLE e746 ADD CONSTRAINT e746_x EXCLUDE USING gist (room WITH =, r WITH &&);
DELETE FROM e746 WHERE id = 4;
ALTER TABLE e746 ADD CONSTRAINT e746_x EXCLUDE USING gist (room WITH =, r WITH &&);
CREATE TABLE f746 (id integer, k integer);
INSERT INTO f746 VALUES (1, 5), (2, NULL), (3, 7), (4, NULL), (5, 7), (6, 5);
ALTER TABLE f746 ADD CONSTRAINT f746_x EXCLUDE USING btree (k WITH =);
REINDEX TABLE g746;
