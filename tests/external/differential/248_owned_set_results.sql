CREATE FUNCTION owned_set_text() RETURNS SETOF text LANGUAGE plpgsql VOLATILE AS $$DECLARE value text := repeat('x',384); BEGIN
    FOR i IN 1..4096 LOOP RETURN NEXT value; END LOOP;
    RETURN;
END$$;
SELECT count(*),min(length(value)),max(length(value)) FROM owned_set_text() AS result(value);
DROP FUNCTION owned_set_text();
CREATE FUNCTION owned_set_prefix(OUT n integer, OUT label text) RETURNS SETOF record LANGUAGE plpgsql VOLATILE AS $$BEGIN
    n := 0; label := 'first'; RETURN NEXT;
    BEGIN
        RETURN QUERY SELECT source.k,
            CASE WHEN source.k=1 THEN 'prefix' ELSE (10/(2-source.k))::text END
            FROM (VALUES(1),(2)) AS source(k);
    EXCEPTION WHEN division_by_zero THEN
        n := 3; label := 'caught'; RETURN NEXT;
    END;
    RETURN;
END$$;
SELECT n,label FROM owned_set_prefix();
DROP FUNCTION owned_set_prefix();
CREATE FUNCTION owned_set_empty() RETURNS SETOF integer LANGUAGE plpgsql VOLATILE AS $$BEGIN RETURN; END$$;
SELECT count(*) FROM owned_set_empty();
DROP FUNCTION owned_set_empty();
CREATE FUNCTION owned_set_shadow(OUT i text, OUT total integer) RETURNS SETOF record LANGUAGE plpgsql VOLATILE AS $$BEGIN
    i := 'outside'; total := 0;
    FOR i IN 1..2 LOOP total := total + i; RETURN NEXT; END LOOP;
    RETURN NEXT;
    RETURN;
END$$;
SELECT i,total FROM owned_set_shadow();
DROP FUNCTION owned_set_shadow();
