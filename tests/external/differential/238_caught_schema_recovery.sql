BEGIN;
CREATE TABLE subtx_durable(id integer PRIMARY KEY);
INSERT INTO subtx_durable VALUES(1);
DO $$BEGIN
  BEGIN
    ALTER TABLE subtx_durable ADD COLUMN discarded integer;
    CREATE TABLE subtx_not_durable(id integer);
    INSERT INTO subtx_durable(id) VALUES(2);
    RAISE EXCEPTION 'discard changes';
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO subtx_durable VALUES(3);
  END;
END$$;
COMMIT;
SELECT id FROM subtx_durable ORDER BY id;
SELECT to_regclass('subtx_not_durable') IS NULL,
       NOT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid='subtx_durable'::regclass AND attname='discarded' AND NOT attisdropped);
