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
-- A one-time filter calling a routine runs once: resumed attempts replay
-- its result with the kept selection.
BEGIN;
CREATE TABLE fdw_once_log (n integer);
CREATE FUNCTION fdw_progress_note(i integer) RETURNS text LANGUAGE plpgsql
  AS $$BEGIN RETURN 'n' || i; END$$;
CREATE FUNCTION fdw_once() RETURNS integer LANGUAGE plpgsql
  AS $$BEGIN INSERT INTO fdw_once_log VALUES (1);
  RAISE NOTICE 'once %', (SELECT count(*) FROM fdw_once_log);
  RETURN (SELECT count(*) FROM fdw_once_log); END$$;
INSERT INTO pos3ql_fdw_source(id, note) VALUES (2005, 'a'), (2006, 'b'), (2007, 'c');
UPDATE pos3ql_fdw_source SET note = fdw_progress_note(id)
  WHERE id > 2000 AND (SELECT fdw_once()) = 1 RETURNING id, note, fdw_progress_note(id + 1);
DELETE FROM pos3ql_fdw_source WHERE id > 2000 AND (SELECT fdw_once()) = 2
  RETURNING id, fdw_progress_note(id + 2);
SELECT count(*) FROM fdw_once_log;
ROLLBACK;
-- A partitioned table with a foreign partition reads and writes the remote
-- rows with the local ones, and a routine call after the first changed row
-- resumes over both: the local rows keep their stored images.
BEGIN;
CREATE FUNCTION fdw_progress_note(i integer) RETURNS text LANGUAGE plpgsql
  AS $$BEGIN RETURN 'n' || i; END$$;
CREATE TABLE fdw_mixed_parent (id integer, note text) PARTITION BY RANGE (id);
CREATE TABLE fdw_mixed_local PARTITION OF fdw_mixed_parent FOR VALUES FROM (2000) TO (3000);
ALTER TABLE fdw_mixed_parent ATTACH PARTITION pos3ql_fdw_mixed FOR VALUES FROM (0) TO (2000);
INSERT INTO fdw_mixed_parent VALUES (1001, 'r1'), (1002, 'r2'), (2001, 'l1'), (2002, 'l2');
SELECT id, note FROM fdw_mixed_parent ORDER BY id;
UPDATE fdw_mixed_parent SET note = fdw_progress_note(id)
  RETURNING id, note, fdw_progress_note(id + 1);
UPDATE fdw_mixed_parent SET note = note || fdw_progress_note(v.x)
  FROM (VALUES (1002), (2002)) v(x) WHERE id = v.x
  RETURNING id, note, v.x;
SELECT id, note FROM fdw_mixed_parent ORDER BY id;
DELETE FROM fdw_mixed_parent WHERE fdw_progress_note(id) <> ''
  RETURNING id, note, fdw_progress_note(id + 2);
SELECT count(*) FROM fdw_mixed_parent;
ROLLBACK;
