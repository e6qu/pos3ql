-- PL/pgSQL FOR rec IN cursor LOOP opens the bound cursor as a portal
-- (visible in pg_cursors), fetches one row per iteration into its own
-- record, pins the portal so CLOSE fails, and closes it on every exit. A
-- cursor variable that was NULL names the portal during the loop and is
-- reset afterwards. COMMIT and ROLLBACK inside a procedure's loop keep the
-- portal readable, as PostgreSQL holds pinned portals.
CREATE TABLE cursor_loop_rows(id int, v text);
INSERT INTO cursor_loop_rows VALUES (1, 'a'), (2, 'b'), (3, 'c');
CREATE FUNCTION cursor_loop_basic() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR (lo int) FOR SELECT id, v FROM cursor_loop_rows WHERE id >= lo ORDER BY id;
  out text := '';
  n int;
BEGIN
  FOR r IN c(2) LOOP
    SELECT count(*) INTO n FROM pg_cursors WHERE name = c::text;
    out := out || r.id || r.v || n || coalesce(c::text, 'null') || found || ';';
  END LOOP;
  RETURN out || found || coalesce(c::text, 'null');
END $$;
SELECT cursor_loop_basic();
CREATE FUNCTION cursor_loop_exits() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR FOR SELECT id FROM cursor_loop_rows ORDER BY id;
  out text := '';
BEGIN
  c := 'named_loop';
  FOR r IN c LOOP EXIT WHEN r.id = 2; out := out || r.id; END LOOP;
  out := out || found || (SELECT count(*) FROM pg_cursors) || c;
  FOR r IN c LOOP CONTINUE WHEN r.id = 2; out := out || r.id; END LOOP;
  <<outer>> FOR i IN 1..2 LOOP
    FOR r IN c LOOP out := out || i || r.id; CONTINUE outer WHEN r.id = 1; END LOOP;
  END LOOP;
  OPEN c;
  BEGIN
    FOR r IN c LOOP out := out || r.id; END LOOP;
  EXCEPTION WHEN duplicate_cursor THEN out := out || ' dup:' || SQLERRM;
  END;
  CLOSE c;
  RETURN out || (SELECT count(*) FROM pg_cursors);
END $$;
SELECT cursor_loop_exits();
CREATE FUNCTION cursor_loop_return() RETURNS int LANGUAGE plpgsql AS $$
DECLARE
  d CURSOR FOR SELECT id FROM cursor_loop_rows ORDER BY id;
  r int := 7;
BEGIN
  FOR r IN d LOOP
    IF r.id = 2 THEN RETURN r.id; END IF;
  END LOOP;
  RETURN 0;
END $$;
SELECT cursor_loop_return(), (SELECT count(*) FROM pg_cursors);
CREATE FUNCTION cursor_loop_shadow() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  d CURSOR FOR SELECT id, id * 2 AS twice FROM cursor_loop_rows ORDER BY id;
  r int := 7;
  x int;
  out text := '';
BEGIN
  FOR r IN d LOOP
    out := out || r.twice;
    FETCH d INTO x;
    out := out || coalesce(x::text, '-');
    d := 'reassigned';
  END LOOP;
  out := out || r || coalesce(d::text, 'null');
  FOR r IN d LOOP out := out || r.id; RAISE EXCEPTION 'boom'; END LOOP;
  RETURN out;
EXCEPTION WHEN others THEN
  RETURN out || ' caught ' || SQLERRM || (SELECT count(*) FROM pg_cursors);
END $$;
SELECT cursor_loop_shadow();
CREATE FUNCTION cursor_loop_empty() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  d SCROLL CURSOR FOR SELECT id FROM cursor_loop_rows WHERE id > 5;
  out text := '';
BEGIN
  PERFORM 1;
  FOR r IN d LOOP out := out || r.id; END LOOP;
  RETURN out || found;
END $$;
SELECT cursor_loop_empty();
CREATE FUNCTION cursor_loop_close() RETURNS void LANGUAGE plpgsql AS $$
DECLARE c CURSOR FOR SELECT id FROM cursor_loop_rows ORDER BY id;
BEGIN
  FOR r IN c LOOP RAISE NOTICE 'row %', r.id; CLOSE c; END LOOP;
END $$;
SELECT cursor_loop_close();
CREATE FUNCTION cursor_loop_nested() RETURNS void LANGUAGE plpgsql AS $$
DECLARE c CURSOR FOR SELECT id FROM cursor_loop_rows ORDER BY id;
BEGIN
  FOR r IN c LOOP FOR s IN c LOOP END LOOP; END LOOP;
END $$;
SELECT cursor_loop_nested() IS NULL;
CREATE FUNCTION cursor_loop_set() RETURNS SETOF int LANGUAGE plpgsql AS $$
DECLARE d CURSOR (k int) FOR SELECT id * k AS p FROM cursor_loop_rows ORDER BY id;
BEGIN
  FOR r IN d(k := 10) LOOP RETURN NEXT r.p; END LOOP;
END $$;
SELECT * FROM cursor_loop_set();
CREATE FUNCTION cursor_loop_no_arguments() RETURNS void LANGUAGE plpgsql AS $$
DECLARE d CURSOR FOR SELECT id FROM cursor_loop_rows;
BEGIN
  FOR r IN d(1) LOOP END LOOP;
END $$;
CREATE FUNCTION cursor_loop_missing_arguments() RETURNS void LANGUAGE plpgsql AS $$
DECLARE d CURSOR (x int) FOR SELECT id FROM cursor_loop_rows;
BEGIN
  FOR r IN d LOOP END LOOP;
END $$;
CREATE FUNCTION cursor_loop_unbound() RETURNS void LANGUAGE plpgsql AS $$
DECLARE d refcursor;
BEGIN
  FOR r IN d LOOP END LOOP;
END $$;
DO $$
DECLARE d CURSOR FOR SELECT id FROM cursor_loop_rows ORDER BY id; t text := '';
BEGIN
  FOR r IN d LOOP t := t || r.id; END LOOP;
  RAISE NOTICE 'do %', t;
END $$;
CREATE TABLE cursor_loop_log(id int);
CREATE PROCEDURE cursor_loop_commit() LANGUAGE plpgsql AS $$
DECLARE d CURSOR FOR SELECT id FROM cursor_loop_rows ORDER BY id;
BEGIN
  FOR r IN d LOOP
    INSERT INTO cursor_loop_log VALUES (r.id);
    IF r.id = 2 THEN ROLLBACK; ELSE COMMIT; END IF;
    RAISE NOTICE 'after % %', r.id, (SELECT count(*) FROM pg_cursors);
  END LOOP;
END $$;
CALL cursor_loop_commit();
SELECT * FROM cursor_loop_log ORDER BY id;
SELECT count(*) FROM pg_cursors;
DROP ROUTINE cursor_loop_basic(), cursor_loop_exits(), cursor_loop_return(), cursor_loop_shadow(), cursor_loop_empty(), cursor_loop_close(), cursor_loop_nested(), cursor_loop_set(), cursor_loop_commit() CASCADE;
