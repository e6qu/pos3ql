-- A routine call after a foreign write's first changed row resumes at the
-- unfinished row: the earlier remote rows stay changed exactly once.
BEGIN;
CREATE FUNCTION fdw_progress_note(i integer) RETURNS text LANGUAGE plpgsql
  AS $$BEGIN RETURN 'n' || i; END$$;
INSERT INTO pos3ql_fdw_source(id, note)
  SELECT g, fdw_progress_note(g) FROM generate_series(2001, 2003) g
  RETURNING id, note, fdw_progress_note(id + 100);
UPDATE pos3ql_fdw_source SET note = fdw_progress_note(id + 10)
  WHERE id > 2000 RETURNING id, note, fdw_progress_note(id);
SELECT id, note FROM pos3ql_fdw_source WHERE id > 2000 ORDER BY id;
DELETE FROM pos3ql_fdw_source WHERE id > 2000 AND fdw_progress_note(id) <> ''
  RETURNING id, fdw_progress_note(id + 20);
INSERT INTO pos3ql_fdw_source(id, note) VALUES (2005, 'a'), (2006, 'b'), (2007, 'c');
UPDATE pos3ql_fdw_source SET note = 'z' || fdw_progress_note(id)
  FROM (VALUES (2005), (2006)) v(x) WHERE id = v.x
  RETURNING id, note, fdw_progress_note(v.x + 1), v.x;
DELETE FROM pos3ql_fdw_source USING (VALUES (2005), (2007)) v(x) WHERE id = v.x
  RETURNING id, fdw_progress_note(v.x + 2), v.x;
SELECT id, note FROM pos3ql_fdw_source WHERE id > 2000 ORDER BY id;
ROLLBACK;
