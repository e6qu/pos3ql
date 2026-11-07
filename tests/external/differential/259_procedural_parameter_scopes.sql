CREATE FUNCTION parameter_diagnostics(SQLSTATE integer, SQLERRM integer) RETURNS text LANGUAGE plpgsql AS $$
DECLARE answer text;
BEGIN
    answer := SQLSTATE::text || ':' || pg_typeof(SQLSTATE)::text || ':' || SQLERRM::text;
    BEGIN
        RAISE EXCEPTION 'parameter handler';
    EXCEPTION WHEN OTHERS THEN
        answer := answer || ':' || SQLSTATE || ':' || SQLERRM;
        FOR SQLSTATE IN 1..1 LOOP
            answer := answer || ':' || SQLSTATE::text || ':' || pg_typeof(SQLSTATE)::text;
        END LOOP;
    END;
    RETURN answer || ':' || SQLSTATE::text || ':' || SQLERRM::text;
END$$;
SELECT parameter_diagnostics(7,8);
DROP FUNCTION parameter_diagnostics(integer,integer);
CREATE FUNCTION parameter_local_shadow(value integer) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE value integer := 42;
BEGIN RETURN value; END$$;
SELECT parameter_local_shadow(7);
DROP FUNCTION parameter_local_shadow(integer);
CREATE FUNCTION parameter_case("SQLSTATE" integer, sqlstate integer) RETURNS text LANGUAGE plpgsql AS $$
BEGIN RETURN "SQLSTATE"::text || ':' || sqlstate::text; END$$;
SELECT parameter_case(7,8);
DROP FUNCTION parameter_case(integer,integer);
CREATE FUNCTION parameter_output_shadow(OUT sqlstate integer,OUT sqlerrm text) LANGUAGE plpgsql AS $$
BEGIN
    sqlstate := 7; sqlerrm := 'output';
    BEGIN RAISE EXCEPTION 'caught';
    EXCEPTION WHEN OTHERS THEN NULL; END;
END$$;
SELECT * FROM parameter_output_shadow();
DROP FUNCTION parameter_output_shadow();
CREATE FUNCTION local_output_shadow(OUT value integer) LANGUAGE plpgsql AS $$
DECLARE value integer := 42;
BEGIN value := value + 1; END$$;
SELECT local_output_shadow();
DROP FUNCTION local_output_shadow();
CREATE FUNCTION local_set_output_shadow() RETURNS TABLE(value integer) LANGUAGE plpgsql AS $$
DECLARE value integer := 42;
BEGIN value := value + 1; RETURN NEXT; END$$;
SELECT * FROM local_set_output_shadow();
DROP FUNCTION local_set_output_shadow();
CREATE PROCEDURE local_procedure_shadow(INOUT value integer) LANGUAGE plpgsql AS $$
DECLARE value integer := 42;
BEGIN value := value + 1; END$$;
CALL local_procedure_shadow(7);
DROP PROCEDURE local_procedure_shadow(integer);
CREATE FUNCTION local_typed_shadow(value integer) RETURNS text LANGUAGE plpgsql AS $$
DECLARE value text := 'local'; answer text; c refcursor;
BEGIN
    SELECT 'static' INTO value;
    answer := value || ':' || pg_typeof(value)::text;
    EXECUTE 'SELECT ''dynamic''' INTO value;
    OPEN c FOR SELECT value;
    value := 'after';
    FETCH c INTO answer;
    CLOSE c;
    FOR value IN 1..1 LOOP
        answer := answer || ':' || value::text || ':' || pg_typeof(value)::text;
    END LOOP;
    RETURN answer || ':' || value || ':' || pg_typeof(value)::text;
END$$;
SELECT local_typed_shadow(7);
DROP FUNCTION local_typed_shadow(integer);
