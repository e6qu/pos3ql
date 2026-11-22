-- PL/pgSQL runs maintenance and session commands as SQL statements, below
-- the top level: LOCK TABLE, SET LOCAL and SET CONSTRAINTS need no
-- transaction block there, and a command that cannot run in a transaction
-- block reports the block, or else the function.
CREATE TABLE m(a int PRIMARY KEY, b int);
INSERT INTO m SELECT g, g FROM generate_series(1, 100) g;
CREATE MATERIALIZED VIEW mv AS SELECT a FROM m;
CREATE FUNCTION maint(e text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE lock int; vacuum text;
BEGIN
  lock := 1;
  vacuum := 'v';
  IF e = 'lock' THEN LOCK TABLE m IN SHARE MODE;
  ELSIF e = 'lock2' THEN LOCK m;
  ELSIF e = 'notify' THEN NOTIFY chan, 'payload';
  ELSIF e = 'listen' THEN LISTEN chan; UNLISTEN chan;
  ELSIF e = 'analyze' THEN ANALYZE m;
  ELSIF e = 'analyze_all' THEN ANALYZE;
  ELSIF e = 'vacuum' THEN VACUUM m;
  ELSIF e = 'vacuum_bare' THEN VACUUM;
  ELSIF e = 'cluster' THEN CLUSTER m USING m_pkey;
  ELSIF e = 'reindex' THEN REINDEX TABLE m;
  ELSIF e = 'refresh' THEN REFRESH MATERIALIZED VIEW mv;
  ELSIF e = 'checkpoint' THEN CHECKPOINT;
  ELSIF e = 'truncate_only' THEN TRUNCATE ONLY m RESTART IDENTITY;
  ELSIF e = 'truncate' THEN TRUNCATE m;
  END IF;
  RETURN e || ' ' || lock || ' ' || vacuum;
END $$;
SELECT maint(e) FROM unnest(ARRAY['lock', 'lock2', 'notify', 'listen', 'analyze',
  'analyze_all', 'reindex', 'refresh', 'cluster', 'checkpoint', 'truncate_only',
  'truncate']) e;
SELECT count(*) FROM m;
BEGIN;
SELECT maint('vacuum');
ROLLBACK;
SELECT maint('vacuum');
SELECT maint('vacuum_bare');
CREATE FUNCTION fx() RETURNS void LANGUAGE plpgsql AS $$ BEGIN EXECUTE 'VACUUM m'; END $$;
CREATE FUNCTION fa() RETURNS void LANGUAGE plpgsql AS $$
BEGIN ALTER SYSTEM SET work_mem = '4MB'; END $$;
CREATE FUNCTION fd() RETURNS void LANGUAGE plpgsql AS $$ BEGIN DISCARD ALL; END $$;
CREATE PROCEDURE pv() LANGUAGE plpgsql AS $$ BEGIN VACUUM m; END $$;
CREATE PROCEDURE ps() LANGUAGE plpgsql AS $$
BEGIN
  BEGIN VACUUM m; EXCEPTION WHEN division_by_zero THEN NULL; END;
END $$;
SELECT fx();
SELECT fa();
SELECT fd();
CALL pv();
CALL ps();
DO $$ BEGIN VACUUM m; END $$;
BEGIN;
CALL pv();
ROLLBACK;
CREATE PROCEDURE pl() LANGUAGE plpgsql AS $$ BEGIN LOCK m; END $$;
CREATE FUNCTION fset() RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  SET LOCAL work_mem = '5MB';
  SET CONSTRAINTS ALL DEFERRED;
  EXECUTE 'LOCK m';
  RETURN current_setting('work_mem');
END $$;
CALL pl();
SELECT fset();
SHOW work_mem;
SET LOCAL work_mem = '6MB';
SET CONSTRAINTS ALL DEFERRED;
LOCK m;
