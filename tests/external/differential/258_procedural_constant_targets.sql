CREATE FUNCTION invalid_handler_assignment() RETURNS void LANGUAGE plpgsql AS $$
DECLARE SQLSTATE integer := 7;
BEGIN BEGIN NULL; EXCEPTION WHEN OTHERS THEN SQLSTATE := '00000'; END; END$$;
CREATE FUNCTION invalid_handler_fetch() RETURNS void LANGUAGE plpgsql AS $$
DECLARE c refcursor;
BEGIN BEGIN NULL; EXCEPTION WHEN OTHERS THEN FETCH c INTO SQLSTATE; END; END$$;
CREATE FUNCTION invalid_constant_fetch() RETURNS void LANGUAGE plpgsql AS $$
DECLARE x constant integer := 1; c refcursor;
BEGIN FETCH c INTO x; END$$;
CREATE FUNCTION invalid_undeclared_counter() RETURNS void LANGUAGE plpgsql AS $$
BEGIN FOR SQLSTATE IN 1..1 LOOP
    BEGIN NULL; EXCEPTION WHEN OTHERS THEN SQLSTATE := '00000'; END;
END LOOP; END$$;
CREATE FUNCTION invalid_scoped_handler_assignment() RETURNS void LANGUAGE plpgsql AS $$
DECLARE SQLSTATE integer := 7;
BEGIN FOR SQLSTATE IN 1..1 LOOP
    BEGIN NULL; EXCEPTION WHEN OTHERS THEN SQLSTATE := '00000'; END;
END LOOP; END$$;
CREATE FUNCTION quoted_diagnostic_counter() RETURNS integer LANGUAGE plpgsql AS $$
BEGIN FOR "sqlstate" IN 1..1 LOOP RETURN "sqlstate"; END LOOP; END$$;
CREATE FUNCTION constant_cursor_open() RETURNS text LANGUAGE plpgsql AS $$
DECLARE c constant refcursor := 'c';
BEGIN OPEN c FOR SELECT 1; CLOSE c; RETURN c; END$$;
CREATE FUNCTION constant_cursor_null() RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE c constant refcursor := NULL;
BEGIN OPEN c FOR SELECT 1; CLOSE c; RETURN c IS NOT NULL; END$$;
CREATE FUNCTION caught_constant_cursor() RETURNS text LANGUAGE plpgsql AS $$
DECLARE c constant refcursor := NULL;
BEGIN
    BEGIN OPEN c FOR SELECT 1;
    EXCEPTION WHEN error_in_assignment THEN RETURN SQLSTATE; END;
    RETURN 'missing error';
END$$;
SELECT count(*) FROM pg_proc WHERE proname IN
    ('invalid_handler_assignment','invalid_handler_fetch','invalid_constant_fetch','invalid_undeclared_counter','invalid_scoped_handler_assignment');
SELECT constant_cursor_open();
SELECT constant_cursor_null();
SELECT quoted_diagnostic_counter();
SELECT caught_constant_cursor();
DROP FUNCTION constant_cursor_open();
DROP FUNCTION constant_cursor_null();
DROP FUNCTION quoted_diagnostic_counter();
DROP FUNCTION caught_constant_cursor();
