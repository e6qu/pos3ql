-- A foreign key's catalog rows across its two tables: the referenced
-- table's relhastriggers, each RI trigger's tgconstrindid (the key's own
-- referenced index, not the first unique index on the same columns), and
-- the dependency rows a later commit leaves unchanged on related tables.
CREATE TABLE c573_parent (id int PRIMARY KEY, code int NOT NULL);
CREATE UNIQUE INDEX c573_parent_id_again ON c573_parent (id);
CREATE TABLE c573_child (id int PRIMARY KEY, parent int REFERENCES c573_parent (id));
CREATE TABLE c573_grandchild (id int PRIMARY KEY, child int REFERENCES c573_child (id));
SELECT relname, relhastriggers FROM pg_class
WHERE relname LIKE 'c573%' AND relkind = 'r' ORDER BY relname;
SELECT t.tgrelid::regclass, i.relname AS index_name, count(*)
FROM pg_trigger t JOIN pg_class i ON i.oid = t.tgconstrindid
WHERE t.tgrelid::regclass::text LIKE 'c573%'
GROUP BY 1, 2 ORDER BY 1, 2;
BEGIN;
ALTER TABLE c573_child ADD COLUMN note text;
CREATE INDEX c573_child_note ON c573_child (note);
COMMIT;
SELECT conname, conrelid::regclass, confrelid::regclass, conindid::regclass
FROM pg_constraint WHERE conrelid::regclass::text LIKE 'c573%' ORDER BY conname;
SELECT classid::regclass, objid = 'c573_child_note'::regclass, refobjsubid, deptype
FROM pg_depend WHERE objid = 'c573_child_note'::regclass ORDER BY refobjsubid;
DROP TABLE c573_grandchild;
SELECT relname, relhastriggers FROM pg_class
WHERE relname LIKE 'c573%' AND relkind = 'r' ORDER BY relname;
CREATE TABLE c573_parts (id int, child int REFERENCES c573_child (id)) PARTITION BY RANGE (id);
CREATE TABLE c573_parts_low PARTITION OF c573_parts FOR VALUES FROM (0) TO (10);
SELECT conname, conrelid::regclass, conparentid <> 0 AS cloned
FROM pg_constraint WHERE conrelid::regclass::text LIKE 'c573_parts%' AND contype = 'f'
ORDER BY conname, conrelid::regclass::text;
SELECT inhrelid::regclass, inhparent::regclass FROM pg_inherits
WHERE inhparent = 'c573_parts'::regclass;
DROP TABLE c573_parts, c573_child, c573_parent;
