CREATE TABLE private_index_source(discarded integer, key integer, payload text);
INSERT INTO private_index_source VALUES (0, 10, 'payload');
BEGIN;
CREATE UNIQUE INDEX private_index_before ON private_index_source(key) INCLUDE(payload);
ALTER TABLE private_index_source DROP COLUMN discarded;
ALTER TABLE private_index_source ADD COLUMN added integer DEFAULT 7;
CREATE UNIQUE INDEX private_index_during ON private_index_source(added) INCLUDE(payload);
SAVEPOINT selected_shape;
ALTER TABLE private_index_source RENAME COLUMN added TO added_renamed;
ALTER TABLE private_index_source RENAME TO private_index_pending;
ALTER TABLE private_index_pending DROP COLUMN key CASCADE;
SELECT payload, added_renamed FROM private_index_pending;
ROLLBACK TO selected_shape;
SELECT key, payload, added FROM private_index_source;
RELEASE selected_shape;
ALTER TABLE private_index_source RENAME TO private_index_final;
ALTER TABLE private_index_final DROP COLUMN key CASCADE;
COMMIT;
SELECT payload, added FROM private_index_final;
DO $body$
BEGIN
  BEGIN
    INSERT INTO private_index_final VALUES ('duplicate', 7);
    RAISE EXCEPTION 'private index lost its unique key after table changes';
  EXCEPTION WHEN SQLSTATE '23505' THEN
    NULL;
  END;
END
$body$;
SELECT count(*) FROM private_index_final;
