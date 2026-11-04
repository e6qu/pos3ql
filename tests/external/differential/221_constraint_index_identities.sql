CREATE TABLE reindex_identity (
  id integer PRIMARY KEY,
  label text UNIQUE,
  left_value integer,
  right_value integer,
  span int4range,
  CONSTRAINT reindex_identity_named UNIQUE(left_value, right_value),
  CONSTRAINT reindex_identity_exclusion EXCLUDE USING gist(span WITH &&)
);
CREATE UNIQUE INDEX reindex_identity_seed ON reindex_identity(right_value, label);
ALTER TABLE reindex_identity ADD CONSTRAINT reindex_identity_attached
  UNIQUE USING INDEX reindex_identity_seed;
INSERT INTO reindex_identity VALUES (1,'one',1,1,'[1,2)'),(2,'two',2,2,'[3,4)');
COMMENT ON CONSTRAINT reindex_identity_pkey ON reindex_identity IS 'constraint retained';
COMMENT ON INDEX reindex_identity_pkey IS 'index retained';
COMMENT ON CONSTRAINT reindex_identity_named ON reindex_identity IS 'named retained';
CREATE TABLE reindex_identity_before AS
  SELECT oid AS constraint_oid, conindid AS index_oid, conparentid AS parent_oid,
         conname, contype
    FROM pg_constraint WHERE conrelid = 'reindex_identity'::regclass
     AND contype IN ('p','u','x');

REINDEX TABLE CONCURRENTLY reindex_identity;
SELECT bool_and(c.oid = b.constraint_oid),
       bool_and((c.contype = 'x' AND c.conindid = b.index_oid)
             OR (c.contype <> 'x' AND c.conindid <> b.index_oid)),
       bool_and(c.conparentid = b.parent_oid)
  FROM pg_constraint c JOIN reindex_identity_before b ON c.oid = b.constraint_oid;
SELECT obj_description(c.oid,'pg_constraint'), obj_description(c.conindid,'pg_class')
  FROM pg_constraint c WHERE c.conrelid = 'reindex_identity'::regclass AND c.contype = 'p';
SELECT count(*) = (SELECT count(*) FROM reindex_identity_before)
  FROM pg_depend d JOIN pg_constraint c ON d.refobjid = c.oid
  JOIN reindex_identity_before b ON c.oid = b.constraint_oid
 WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_constraint'::regclass
   AND d.objid = c.conindid AND d.deptype = 'i';

ALTER INDEX reindex_identity_named RENAME TO reindex_identity_named_renamed;
SELECT c.oid = b.constraint_oid, obj_description(c.oid,'pg_constraint')
  FROM pg_constraint c JOIN reindex_identity_before b ON c.oid = b.constraint_oid
 WHERE c.conname = 'reindex_identity_named_renamed';

CREATE TABLE reindex_identity_partition(id integer PRIMARY KEY) PARTITION BY RANGE(id);
CREATE TABLE reindex_identity_low PARTITION OF reindex_identity_partition FOR VALUES FROM(0) TO(10);
CREATE TABLE reindex_identity_high PARTITION OF reindex_identity_partition FOR VALUES FROM(10) TO(20);
CREATE TABLE reindex_identity_partition_before AS
  SELECT c.oid AS constraint_oid, c.conindid AS index_oid, c.conparentid AS parent_oid,
         i.relkind
    FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid
   WHERE c.conrelid IN ('reindex_identity_partition'::regclass,
                        'reindex_identity_low'::regclass,'reindex_identity_high'::regclass)
     AND c.contype = 'p';
REINDEX INDEX CONCURRENTLY reindex_identity_partition_pkey;
SELECT bool_and(c.oid = b.constraint_oid), bool_and(c.conparentid = b.parent_oid),
       bool_and((b.relkind = 'I' AND c.conindid = b.index_oid)
             OR (b.relkind <> 'I' AND c.conindid <> b.index_oid))
  FROM pg_constraint c JOIN reindex_identity_partition_before b ON c.oid = b.constraint_oid;
SELECT count(*) = 2
  FROM pg_constraint c JOIN pg_constraint p ON p.oid = c.conparentid
  JOIN pg_inherits i ON i.inhrelid = c.conindid AND i.inhparent = p.conindid
 WHERE p.conrelid = 'reindex_identity_partition'::regclass;

CREATE TABLE reindex_identity_drop_log(classid oid, object_id oid, name text);
CREATE FUNCTION capture_reindex_identity_drop() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO reindex_identity_drop_log
     SELECT classid, objid, object_name FROM pg_event_trigger_dropped_objects(); RETURN; END';
CREATE EVENT TRIGGER capture_reindex_identity_drop ON sql_drop
  EXECUTE FUNCTION capture_reindex_identity_drop();
ALTER TABLE reindex_identity DROP CONSTRAINT reindex_identity_named_renamed;
SELECT count(*) = 1 FROM reindex_identity_drop_log d
  JOIN reindex_identity_before b ON d.object_id = b.constraint_oid
 WHERE d.classid = 'pg_constraint'::regclass AND b.conname = 'reindex_identity_named';
SELECT count(*) = 1 FROM reindex_identity_drop_log WHERE classid = 'pg_class'::regclass;
CREATE INDEX reindex_identity_plain ON reindex_identity(left_value);
BEGIN;
DROP INDEX reindex_identity_plain;
TRUNCATE reindex_identity_drop_log;
DROP TABLE reindex_identity;
SELECT count(*) = count(DISTINCT object_id)
  FROM reindex_identity_drop_log WHERE classid = 'pg_class'::regclass;
SELECT count(*) = 0 FROM reindex_identity_drop_log WHERE name = 'reindex_identity_plain';
SELECT count(*) = 3 FROM reindex_identity_drop_log d
  JOIN reindex_identity_before b ON d.object_id = b.constraint_oid
 WHERE d.classid = 'pg_constraint'::regclass AND b.contype IN ('p','u');
ROLLBACK;
DROP EVENT TRIGGER capture_reindex_identity_drop;
DROP FUNCTION capture_reindex_identity_drop();
DROP TABLE reindex_identity, reindex_identity_before, reindex_identity_partition,
  reindex_identity_partition_before, reindex_identity_drop_log;
