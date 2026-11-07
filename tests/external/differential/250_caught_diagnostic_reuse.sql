CREATE FUNCTION caught_diagnostic_reuse() RETURNS text LANGUAGE plpgsql VOLATILE AS $$
DECLARE payload text := repeat('x',8192); saved text := ''; caught integer := 0;
BEGIN
    FOR i IN 1..4096 LOOP
        BEGIN
            RAISE EXCEPTION 'error %',i USING DETAIL = payload;
        EXCEPTION WHEN OTHERS THEN
            caught := caught + 1;
            IF i=1 THEN saved := SQLERRM; END IF;
        END;
    END LOOP;
    RETURN caught::text || ':' || saved;
END$$;
SELECT caught_diagnostic_reuse();
DROP FUNCTION caught_diagnostic_reuse();
CREATE FUNCTION nested_diagnostic_reuse() RETURNS text LANGUAGE plpgsql VOLATILE AS $$
DECLARE saved text; inner_message text; outer_detail text; outer_hint text;
    empty_detail text; empty_hint text; empty_column text; code text;
BEGIN
    BEGIN
        RAISE EXCEPTION 'outer' USING DETAIL = 'outer detail', HINT = 'outer hint';
    EXCEPTION WHEN OTHERS THEN
        saved := SQLERRM;
        BEGIN
            BEGIN
                RAISE EXCEPTION 'inner';
            EXCEPTION WHEN OTHERS THEN
                GET STACKED DIAGNOSTICS empty_detail=PG_EXCEPTION_DETAIL,
                    empty_hint=PG_EXCEPTION_HINT,empty_column=COLUMN_NAME;
                RAISE;
            END;
        EXCEPTION WHEN OTHERS THEN
            inner_message := SQLERRM;
        END;
        GET STACKED DIAGNOSTICS outer_detail=PG_EXCEPTION_DETAIL,
            outer_hint=PG_EXCEPTION_HINT,code=RETURNED_SQLSTATE;
        RETURN saved || ':' || inner_message || ':' || SQLERRM || ':' || code
            || ':' || outer_detail || ':' || outer_hint || ':'
            || (empty_detail='')::text || ':' || (empty_hint='')::text
            || ':' || (empty_column='')::text;
    END;
END$$;
SELECT nested_diagnostic_reuse();
DROP FUNCTION nested_diagnostic_reuse();
