CREATE TABLE remapped_key_parent(id integer PRIMARY KEY, code integer, UNIQUE(id,code)) PARTITION BY RANGE(id);
CREATE TABLE remapped_key_child(code integer, id integer NOT NULL);
ALTER TABLE remapped_key_parent ATTACH PARTITION remapped_key_child FOR VALUES FROM(0) TO(10);
INSERT INTO remapped_key_parent VALUES(1,7);
SELECT 'before', c.contype, c.conparentid = p.oid, i.inhparent = p.conindid, c.conkey::text, p.conkey::text
 FROM pg_constraint c JOIN pg_constraint p ON p.conrelid = 'remapped_key_parent'::regclass AND p.contype = c.contype
 LEFT JOIN pg_inherits i ON i.inhrelid = c.conindid
 WHERE c.conrelid = 'remapped_key_child'::regclass AND c.contype IN ('p','u') ORDER BY c.contype;
CREATE TABLE remapped_key_before AS SELECT oid AS constraint_oid, conindid AS index_oid, conparentid AS parent_oid
 FROM pg_constraint WHERE conrelid = 'remapped_key_child'::regclass AND contype IN ('p','u');
REINDEX TABLE CONCURRENTLY remapped_key_parent;
SELECT 'after', c.contype, c.conparentid = p.oid, i.inhparent = p.conindid, c.conkey::text, p.conkey::text
 FROM pg_constraint c JOIN pg_constraint p ON p.conrelid = 'remapped_key_parent'::regclass AND p.contype = c.contype
 LEFT JOIN pg_inherits i ON i.inhrelid = c.conindid
 WHERE c.conrelid = 'remapped_key_child'::regclass AND c.contype IN ('p','u') ORDER BY c.contype;
SELECT 'retained', count(*) = 2, bool_and(c.oid = b.constraint_oid), bool_and(c.conparentid = b.parent_oid), bool_and(c.conindid <> b.index_oid)
 FROM pg_constraint c JOIN remapped_key_before b ON c.oid = b.constraint_oid;
SELECT 'row',id,code FROM remapped_key_parent;
DROP TABLE remapped_key_parent,remapped_key_before;
