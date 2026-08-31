CREATE TABLE attached_index_constraint (id integer, code integer);
CREATE UNIQUE INDEX attached_index_constraint_id_idx ON attached_index_constraint (id);
CREATE UNIQUE INDEX attached_index_constraint_code_idx ON attached_index_constraint (code);

ALTER TABLE attached_index_constraint
  ADD CONSTRAINT attached_index_constraint_pkey
  PRIMARY KEY USING INDEX attached_index_constraint_id_idx;
ALTER TABLE attached_index_constraint
  ADD UNIQUE USING INDEX attached_index_constraint_code_idx;

SELECT conname, contype,
       conindid = (SELECT oid FROM pg_class WHERE relname = conname)
FROM pg_constraint
WHERE conrelid = 'attached_index_constraint'::regclass
ORDER BY conname;

INSERT INTO attached_index_constraint VALUES (1, 1);
INSERT INTO attached_index_constraint VALUES (1, 2);
DROP INDEX attached_index_constraint_pkey;

ALTER INDEX attached_index_constraint_pkey
  RENAME TO attached_index_constraint_primary;
SELECT relname
FROM pg_class
WHERE relname IN ('attached_index_constraint_pkey', 'attached_index_constraint_primary');
ALTER TABLE attached_index_constraint DROP CONSTRAINT attached_index_constraint_primary;
SELECT count(*) FROM pg_class WHERE relname = 'attached_index_constraint_primary';
