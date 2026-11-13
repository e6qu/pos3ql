-- The tableoid system column names the physical table each row came from:
-- the table itself, or the partition, inheritance child or descendant that
-- holds the row. It is hidden from star expansion and absent from views,
-- subqueries and functions.
CREATE TABLE toid_plain (a int, b text);
INSERT INTO toid_plain VALUES (1, 'x'), (2, 'y');
SELECT tableoid::regclass, a FROM toid_plain ORDER BY a;
SELECT pg_typeof(tableoid), tableoid = 'toid_plain'::regclass FROM toid_plain LIMIT 1;
SELECT * FROM toid_plain ORDER BY a;
SELECT t.tableoid::regclass, t.b FROM toid_plain t WHERE t.tableoid = 'toid_plain'::regclass ORDER BY 2;
SELECT count(*) FROM toid_plain WHERE tableoid <> 'toid_plain'::regclass;
SELECT tableoid::regclass, count(*) FROM toid_plain GROUP BY tableoid ORDER BY 1;
SELECT p.tableoid::regclass, q.tableoid::regclass, p.a FROM toid_plain p JOIN toid_plain q USING (a) ORDER BY 3;
SELECT tableoid FROM (SELECT * FROM toid_plain) s;
CREATE VIEW toid_view AS SELECT a FROM toid_plain;
SELECT tableoid FROM toid_view;
SELECT tableoid FROM generate_series(1, 2) g;
SELECT x.tableoid FROM toid_plain p, LATERAL (SELECT p.a) x(a);
SELECT (SELECT max(tableoid::regclass::text) FROM toid_plain WHERE a = o.a) FROM toid_plain o ORDER BY o.a;
-- Partitions and their sub-partitions.
CREATE TABLE toid_part (k int, v text) PARTITION BY RANGE (k);
CREATE TABLE toid_part_lo PARTITION OF toid_part FOR VALUES FROM (0) TO (10);
CREATE TABLE toid_part_hi PARTITION OF toid_part FOR VALUES FROM (10) TO (100) PARTITION BY LIST (v);
CREATE TABLE toid_part_hi_a PARTITION OF toid_part_hi FOR VALUES IN ('a');
CREATE TABLE toid_part_hi_b PARTITION OF toid_part_hi FOR VALUES IN ('b');
INSERT INTO toid_part VALUES (1, 'a'), (11, 'a'), (12, 'b') RETURNING tableoid::regclass, k;
SELECT tableoid::regclass, k, v FROM toid_part ORDER BY k;
SELECT tableoid::regclass, k FROM toid_part_hi ORDER BY k;
SELECT tableoid::regclass, count(*) FROM toid_part GROUP BY 1 ORDER BY 1;
SELECT k FROM toid_part WHERE tableoid = 'toid_part_hi_b'::regclass;
UPDATE toid_part SET v = 'b' WHERE k = 11 RETURNING tableoid::regclass, k, v;
UPDATE toid_part SET k = k + 20 WHERE tableoid = 'toid_part_lo'::regclass RETURNING tableoid::regclass, k;
DELETE FROM toid_part WHERE tableoid = 'toid_part_hi_b'::regclass AND k = 12 RETURNING tableoid::regclass, k;
SELECT tableoid::regclass, k, v FROM toid_part ORDER BY k;
-- Inheritance: ONLY reads the parent's own rows.
CREATE TABLE toid_parent (id int);
CREATE TABLE toid_child (extra text) INHERITS (toid_parent);
CREATE TABLE toid_grandchild () INHERITS (toid_child);
INSERT INTO toid_parent VALUES (1);
INSERT INTO toid_child VALUES (2, 'c');
INSERT INTO toid_grandchild VALUES (3, 'g');
SELECT tableoid::regclass, id FROM toid_parent ORDER BY id;
SELECT tableoid::regclass, id FROM ONLY toid_parent ORDER BY id;
SELECT tableoid::regclass, id, extra FROM toid_child ORDER BY id;
UPDATE toid_parent SET id = id * 10 WHERE tableoid <> 'toid_parent'::regclass RETURNING tableoid::regclass, id;
DELETE FROM toid_parent WHERE tableoid = 'toid_grandchild'::regclass RETURNING tableoid::regclass, id;
INSERT INTO toid_child VALUES (4, 'd') RETURNING tableoid::regclass, *;
SELECT tableoid::regclass, id FROM toid_parent ORDER BY id;
-- Data-changing statements over a plain table.
INSERT INTO toid_plain VALUES (3, 'z') RETURNING tableoid::regclass, a;
UPDATE toid_plain SET b = b || '!' WHERE tableoid = 'toid_plain'::regclass AND a = 3 RETURNING tableoid::regclass, b;
DELETE FROM toid_plain WHERE a = 3 RETURNING tableoid::regclass;
INSERT INTO toid_plain VALUES (1, 'dup') ON CONFLICT DO NOTHING RETURNING tableoid::regclass;
CREATE TABLE toid_other (id int);
INSERT INTO toid_other VALUES (1);
UPDATE toid_plain AS t SET b = 'u' FROM toid_other o WHERE t.a = 2 AND o.id = 1 RETURNING t.tableoid::regclass, o.tableoid::regclass, tableoid;
UPDATE toid_part AS t SET v = 'a' FROM toid_other o WHERE t.k = 11 AND o.id = 1 RETURNING t.tableoid::regclass, o.tableoid::regclass;
DELETE FROM toid_plain t USING toid_other o WHERE t.a = 2 AND o.id = 1 RETURNING t.tableoid::regclass, o.tableoid::regclass;
-- Joins, outer joins, whole rows, windows, cursors and subqueries.
SELECT p.tableoid::regclass, o.tableoid::regclass, p.id FROM toid_parent p LEFT JOIN toid_other o ON o.id = p.id ORDER BY p.id;
SELECT tableoid FROM toid_parent p, toid_other o;
SELECT p, p.* FROM toid_parent p ORDER BY id;
SELECT tableoid::regclass, row_number() OVER (ORDER BY a) FROM toid_plain;
SELECT count(DISTINCT tableoid) FROM toid_parent;
SELECT id FROM toid_parent p WHERE EXISTS (SELECT 1 FROM toid_other o WHERE p.tableoid = 'toid_parent'::regclass AND o.id = p.id);
SELECT id FROM toid_parent WHERE tableoid IN (SELECT 'toid_child'::regclass::oid) ORDER BY id;
BEGIN;
DECLARE toid_cursor CURSOR FOR SELECT tableoid::regclass, a FROM toid_plain ORDER BY a;
FETCH 2 FROM toid_cursor;
COMMIT;
CREATE VIEW toid_view2 AS SELECT tableoid AS source, id FROM toid_parent;
SELECT source::regclass, id FROM toid_view2 ORDER BY id;
INSERT INTO toid_view2 (id) VALUES (7) RETURNING source::regclass;
INSERT INTO toid_view VALUES (9) RETURNING toid_view.tableoid;
INSERT INTO toid_plain VALUES (8) RETURNING old.tableoid::regclass, new.tableoid::regclass;
DELETE FROM toid_plain WHERE a = 8 RETURNING old.tableoid::regclass, new.tableoid::regclass;
SELECT old.tableoid, new.tableoid FROM toid_plain;
UPDATE toid_plain SET b = 'v' WHERE a = 1 RETURNING old.tableoid::regclass, new.tableoid::regclass;
-- A column cannot take the name.
CREATE TABLE toid_bad (tableoid int);
ALTER TABLE toid_plain ADD COLUMN tableoid int;
