CREATE FUNCTION saved_output_numeric_scopes() RETURNS TABLE(v text,n integer) LANGUAGE plpgsql AS $$
BEGIN
    n := 0;
    FOR turn IN 1..256 LOOP
        v := repeat(chr(65 + turn % 26),384);
        FOR v IN 1..2 LOOP
            n := n + 1;
            RETURN NEXT;
            FOR v IN 1..2 LOOP
                n := n + 1;
                RETURN NEXT;
            END LOOP;
        END LOOP;
    END LOOP;
    RETURN NEXT;
END$$;
SELECT count(*),min(length(v)),max(length(v)),min(n),max(n),sum(n) FROM saved_output_numeric_scopes();
DROP FUNCTION saved_output_numeric_scopes();

CREATE FUNCTION saved_output_caught_scope() RETURNS TABLE(v text,n integer) LANGUAGE plpgsql AS $$
BEGIN
    v := 'initial';
    n := 0;
    BEGIN
        FOR v IN 1..1 LOOP
            n := 7;
            RETURN NEXT;
            RAISE division_by_zero;
        END LOOP;
    EXCEPTION WHEN division_by_zero THEN
        RETURN NEXT;
    END;
    v := NULL;
    FOR v IN 1..1 LOOP
        RETURN NEXT;
    END LOOP;
END$$;
SELECT * FROM saved_output_caught_scope();
DROP FUNCTION saved_output_caught_scope();

CREATE TYPE saved_output_record AS (label text,amounts integer[]);
CREATE FUNCTION saved_output_typed_scope() RETURNS TABLE(v saved_output_record,n integer) LANGUAGE plpgsql AS $$
BEGIN
    v := ROW('outer','[-2:-1]={7,NULL}'::integer[])::saved_output_record;
    FOR v IN 1..2 LOOP
        n := v;
        RETURN NEXT;
    END LOOP;
END$$;
SELECT (v).label,(v).amounts,array_lower((v).amounts,1),n FROM saved_output_typed_scope();
DROP FUNCTION saved_output_typed_scope();
DROP TYPE saved_output_record;
