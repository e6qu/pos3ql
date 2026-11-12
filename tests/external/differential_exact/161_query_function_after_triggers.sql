-- AFTER triggers of writes in a function called from a query run on the
-- engine: their routine calls, DDL and statement triggers work as in
-- PostgreSQL, for PL/pgSQL and SQL-language functions.
CREATE SCHEMA query_after;
SET search_path = query_after, public;
CREATE SEQUENCE q; CREATE TABLE qt(a int); CREATE TABLE qlog(m text, k bigint);
CREATE FUNCTION note(x text) RETURNS int LANGUAGE plpgsql AS $$BEGIN INSERT INTO qlog VALUES (x, nextval('q')); RETURN 1; END$$;
CREATE FUNCTION aft() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN
  IF current_setting('application_name') = 'wait' THEN PERFORM pg_advisory_xact_lock(7); END IF;
  PERFORM note('after ' || NEW.a || ' ' || (SELECT count(*) FROM qt));
  EXECUTE 'CREATE TABLE IF NOT EXISTS query_after_made_' || NEW.a || '(x int)';
  RETURN NULL; END$$;
CREATE TRIGGER ta AFTER INSERT ON qt FOR EACH ROW EXECUTE FUNCTION aft();
CREATE FUNCTION w(n int) RETURNS bigint LANGUAGE plpgsql AS $$BEGIN INSERT INTO qt VALUES (n), (n + 1); RETURN (SELECT count(*) FROM qlog); END$$;
SELECT w(1);
SELECT w(10) FROM generate_series(1, 2);
SELECT m, k FROM qlog ORDER BY k;
SELECT relname FROM pg_class WHERE relname LIKE 'query_after_made_%' ORDER BY relname;
CREATE FUNCTION s(n int) RETURNS bigint LANGUAGE sql AS $$INSERT INTO qt VALUES (n); SELECT count(*) FROM qlog$$;
SELECT s(20);
CREATE FUNCTION aftstmt() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN PERFORM note('stmt ' || TG_OP); RETURN NULL; END$$;
CREATE TRIGGER tb AFTER UPDATE ON qt FOR EACH STATEMENT EXECUTE FUNCTION aftstmt();
CREATE FUNCTION u() RETURNS bigint LANGUAGE plpgsql AS $$BEGIN UPDATE qt SET a = a WHERE a = 1; RETURN (SELECT count(*) FROM qlog); END$$;
SELECT u(), u();
SELECT m, k FROM qlog ORDER BY k;
-- A function calling such a function, and an exception block catching an
-- AFTER trigger's error, which undoes the write and the trigger's work.
CREATE FUNCTION outer_w(n int) RETURNS bigint LANGUAGE plpgsql AS $$BEGIN PERFORM w(n); RETURN (SELECT count(*) FROM qt); END$$;
SELECT outer_w(30);
CREATE FUNCTION bad() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN PERFORM note('bad ' || NEW.a); IF NEW.a = 41 THEN RAISE EXCEPTION 'reject %', NEW.a; END IF; RETURN NULL; END$$;
CREATE TRIGGER tz AFTER INSERT ON qt FOR EACH ROW EXECUTE FUNCTION bad();
CREATE FUNCTION guarded(n int) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  BEGIN
    INSERT INTO qt VALUES (n), (n + 1);
  EXCEPTION WHEN raise_exception THEN
    RETURN 'caught ' || SQLERRM;
  END;
  RETURN 'kept';
END$$;
SELECT guarded(40);
SELECT guarded(50);
SELECT w(41);
SELECT m, k FROM qlog ORDER BY k;
SELECT a FROM qt ORDER BY a;
SELECT relname FROM pg_class WHERE relname LIKE 'query_after_made_%' ORDER BY relname;
RESET search_path;
DROP SCHEMA query_after CASCADE;
