CREATE FUNCTION diagnostic_counter_scopes() RETURNS text LANGUAGE plpgsql AS $$
DECLARE SQLSTATE integer := 7; SQLERRM integer := 8; result text := '';
BEGIN
    BEGIN
        RAISE EXCEPTION 'caught';
    EXCEPTION WHEN OTHERS THEN
        FOR SQLSTATE IN 1..2 LOOP
            result := result || SQLSTATE::text || ':' || pg_typeof(SQLSTATE)::text || '|';
        END LOOP;
        FOR SQLERRM IN 3..4 LOOP
            result := result || SQLERRM::text || ':' || pg_typeof(SQLERRM)::text || '|';
        END LOOP;
        result := result || SQLSTATE || ':' || SQLERRM;
    END;
    result := result || ':' || SQLSTATE::text || ':' || SQLERRM::text;
    FOR SQLSTATE IN 5..5 LOOP
        BEGIN
            RAISE EXCEPTION 'inner';
        EXCEPTION WHEN OTHERS THEN
            result := result || ':' || SQLSTATE || ':' || pg_typeof(SQLSTATE)::text;
        END;
        result := result || ':' || SQLSTATE::text || ':' || pg_typeof(SQLSTATE)::text;
    END LOOP;
    RETURN result;
END$$;
SELECT diagnostic_counter_scopes();
DROP FUNCTION diagnostic_counter_scopes();
