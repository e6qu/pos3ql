-- Nested SQL calls retain checked inputs and effects in every PL query form.
CREATE SEQUENCE procedural_query_checks;
CREATE SEQUENCE procedural_query_calls;
CREATE TYPE procedural_query_choice AS ENUM('a');
CREATE DOMAIN procedural_query_checked AS procedural_query_choice
  CHECK(nextval('procedural_query_checks')>0);
CREATE TABLE procedural_query_log(position serial,kind text,value bigint);
CREATE FUNCTION procedural_query_value(input integer) RETURNS integer
LANGUAGE sql VOLATILE SET search_path=public AS $$
  SELECT input+cardinality('{a}'::procedural_query_checked[])
    +nextval('procedural_query_calls')::integer
$$;
CREATE PROCEDURE procedural_query_run() LANGUAGE plpgsql AS $$
DECLARE selected integer; affected bigint; item record;
BEGIN
  PERFORM procedural_query_value(v) FROM generate_series(1,3) source(v);
  GET DIAGNOSTICS affected=ROW_COUNT;
  INSERT INTO procedural_query_log(kind,value) VALUES('perform',affected);
  SELECT procedural_query_value(1) INTO selected;
  INSERT INTO procedural_query_log(kind,value) VALUES('select',selected);
  FOR selected IN SELECT procedural_query_value(v) FROM generate_series(1,2) source(v) LOOP
    INSERT INTO procedural_query_log(kind,value) VALUES('for',selected);
  END LOOP;
  FOR item IN SELECT procedural_query_value(v) AS value FROM generate_series(1,2) source(v) LOOP
    INSERT INTO procedural_query_log(kind,value) VALUES('record',item.value);
  END LOOP;
  EXECUTE 'SELECT procedural_query_value(1)' INTO selected;
  INSERT INTO procedural_query_log(kind,value) VALUES('execute_into',selected);
  FOR selected IN EXECUTE 'SELECT procedural_query_value(v) FROM generate_series(1,2) source(v)' LOOP
    INSERT INTO procedural_query_log(kind,value) VALUES('execute_for',selected);
  END LOOP;
  FOR item IN EXECUTE 'SELECT procedural_query_value(v) AS value FROM generate_series(1,2) source(v)' LOOP
    INSERT INTO procedural_query_log(kind,value) VALUES('execute_record',item.value);
  END LOOP;
  EXECUTE 'SELECT procedural_query_value(1)';
  GET DIAGNOSTICS affected=ROW_COUNT;
  INSERT INTO procedural_query_log(kind,value) VALUES('execute_discard',affected);
END
$$;
CREATE FUNCTION procedural_query_rows() RETURNS SETOF integer LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT procedural_query_value(v) FROM generate_series(1,2) source(v);
  RETURN QUERY EXECUTE 'SELECT procedural_query_value(v) FROM generate_series(1,2) source(v)';
END
$$;
CREATE FUNCTION procedural_query_caught() RETURNS SETOF integer LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT procedural_query_value(v)+10/(2-v) FROM generate_series(1,2) source(v);
EXCEPTION WHEN division_by_zero THEN
  RETURN QUERY SELECT 99;
END
$$;
CREATE FUNCTION procedural_query_caught_dynamic() RETURNS SETOF integer LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY EXECUTE 'SELECT procedural_query_value(v)+10/(2-v) FROM generate_series(1,2) source(v)';
EXCEPTION WHEN division_by_zero THEN
  RETURN QUERY SELECT 98;
END
$$;
ALTER SEQUENCE procedural_query_checks RESTART WITH 1;
ALTER SEQUENCE procedural_query_calls RESTART WITH 1;
-- Execute and inspect the completed results.
CALL procedural_query_run();
SELECT kind,value FROM procedural_query_log ORDER BY position;
SELECT last_value FROM procedural_query_calls;
SELECT last_value FROM procedural_query_checks;
SELECT * FROM procedural_query_rows();
SELECT last_value FROM procedural_query_calls;
SELECT last_value FROM procedural_query_checks;
SELECT * FROM procedural_query_caught();
SELECT last_value FROM procedural_query_calls;
SELECT * FROM procedural_query_caught_dynamic();
SELECT last_value FROM procedural_query_calls;
SELECT last_value FROM procedural_query_checks;
DROP PROCEDURE procedural_query_run();
DROP FUNCTION procedural_query_rows(),procedural_query_caught(),procedural_query_caught_dynamic();
DROP FUNCTION procedural_query_value(integer);
DROP TABLE procedural_query_log;
DROP DOMAIN procedural_query_checked;
DROP TYPE procedural_query_choice;
DROP SEQUENCE procedural_query_checks,procedural_query_calls;

-- Bounded INTO setup.
CREATE SEQUENCE procedural_into_calls;
CREATE TABLE procedural_into_log(kind text,selected integer,effects bigint,consumed bigint);
CREATE FUNCTION procedural_into_value(input integer) RETURNS integer
  LANGUAGE sql VOLATILE SET search_path=public
  AS $$SELECT nextval('procedural_into_calls')::integer$$;
CREATE PROCEDURE procedural_into_run() LANGUAGE plpgsql AS $$
DECLARE selected integer; consumed bigint;
BEGIN
  SELECT procedural_into_value(v) INTO selected FROM generate_series(1,3) source(v);
  GET DIAGNOSTICS consumed=ROW_COUNT;
  INSERT INTO procedural_into_log VALUES('static',selected,currval('procedural_into_calls'),consumed);
  BEGIN
    SELECT procedural_into_value(v) INTO STRICT selected FROM generate_series(1,3) source(v);
  EXCEPTION WHEN too_many_rows THEN
    GET DIAGNOSTICS consumed=ROW_COUNT;
    INSERT INTO procedural_into_log VALUES('static_strict',selected,currval('procedural_into_calls'),consumed);
  END;
  EXECUTE 'SELECT procedural_into_value(v) FROM generate_series(1,3) source(v)' INTO selected;
  GET DIAGNOSTICS consumed=ROW_COUNT;
  INSERT INTO procedural_into_log VALUES('dynamic',selected,currval('procedural_into_calls'),consumed);
  BEGIN
    EXECUTE 'SELECT procedural_into_value(v) FROM generate_series(1,3) source(v)' INTO STRICT selected;
  EXCEPTION WHEN too_many_rows THEN
    GET DIAGNOSTICS consumed=ROW_COUNT;
    INSERT INTO procedural_into_log VALUES('dynamic_strict',selected,currval('procedural_into_calls'),consumed);
  END;
END
$$;
-- Execute bounded INTO cases.
CALL procedural_into_run();
SELECT kind,selected,effects,consumed FROM procedural_into_log ORDER BY kind;
DROP PROCEDURE procedural_into_run();
DROP FUNCTION procedural_into_value(integer);
DROP TABLE procedural_into_log;
DROP SEQUENCE procedural_into_calls;
