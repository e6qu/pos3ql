-- ALTER TABLE control-plane commands update durable catalog state without
-- rewriting rows or leaving stale cluster selections.

DROP TABLE IF EXISTS alter_control_rows;

CREATE TABLE alter_control_rows (id integer, payload text);
INSERT INTO alter_control_rows VALUES (2, 'two'), (1, 'one');
CREATE INDEX alter_control_rows_id_idx ON alter_control_rows (id);

ALTER TABLE alter_control_rows CLUSTER ON alter_control_rows_id_idx;
SELECT indisclustered
  FROM pg_index
 WHERE indexrelid = 'alter_control_rows_id_idx'::regclass;

ALTER TABLE alter_control_rows SET WITHOUT CLUSTER;
SELECT indisclustered
  FROM pg_index
 WHERE indexrelid = 'alter_control_rows_id_idx'::regclass;

ALTER TABLE alter_control_rows SET WITHOUT OIDS;
ALTER TABLE alter_control_rows ADD COLUMN IF NOT EXISTS label text DEFAULT 'new';
ALTER TABLE alter_control_rows ADD COLUMN IF NOT EXISTS label integer;
ALTER TABLE alter_control_rows SET ACCESS METHOD DEFAULT;

SELECT id, payload, label FROM alter_control_rows ORDER BY id;
SELECT attname, atttypid::regtype::text
  FROM pg_attribute
 WHERE attrelid = 'alter_control_rows'::regclass AND attnum > 0
 ORDER BY attnum;

CREATE TYPE alter_control_row AS (id integer, payload text, label text);
ALTER TABLE alter_control_rows OF alter_control_row;
SELECT c.reloftype = t.oid
  FROM pg_class c JOIN pg_type t ON t.typname = 'alter_control_row'
 WHERE c.relname = 'alter_control_rows';
ALTER TABLE alter_control_rows NOT OF;
SELECT reloftype = 0 FROM pg_class WHERE relname = 'alter_control_rows';
DROP TYPE alter_control_row;

DROP TABLE alter_control_rows;
