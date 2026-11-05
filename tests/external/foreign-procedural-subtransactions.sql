-- The first foreign connection can open inside the failed block. Nested
-- catches must mirror its transaction boundaries without losing the outer row.
BEGIN;
SAVEPOINT outside_procedure;
DO $$BEGIN
  BEGIN
    INSERT INTO pos3ql_fdw_source(id,note) VALUES(1001,'discarded first connection');
    RAISE EXCEPTION 'discard remote insert';
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO pos3ql_fdw_source(id,note) VALUES(1002,'kept handler');
  END;
  BEGIN
    UPDATE pos3ql_fdw_source SET note='discarded outer change' WHERE id=1002;
    BEGIN
      DELETE FROM pos3ql_fdw_source WHERE id=1002;
      RAISE EXCEPTION 'restore remote delete';
    EXCEPTION WHEN raise_exception THEN
      INSERT INTO pos3ql_fdw_source(id,note) VALUES(1003,'discarded nested handler');
    END;
    RAISE EXCEPTION 'discard the enclosing remote changes';
  EXCEPTION WHEN raise_exception THEN NULL;
  END;
END$$;
SELECT id,note FROM pos3ql_fdw_source WHERE id>=1000 ORDER BY id;
ROLLBACK TO outside_procedure;
SELECT count(*) FROM pos3ql_fdw_source WHERE id>=1000;
COMMIT;
