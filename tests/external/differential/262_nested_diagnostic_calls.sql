CREATE SEQUENCE nested_diagnostic_calls;
CREATE TABLE nested_diagnostic_audit(id integer);
CREATE FUNCTION nested_diagnostic_child(p integer) RETURNS text LANGUAGE plpgsql AS $$
DECLARE outer_message text; inner_message text; outer_detail text;
BEGIN
    BEGIN
        RAISE EXCEPTION 'outer %',p USING DETAIL = repeat('d',8192);
    EXCEPTION WHEN OTHERS THEN
        outer_message := SQLERRM;
        BEGIN
            RAISE EXCEPTION 'inner';
        EXCEPTION WHEN OTHERS THEN
            inner_message := SQLERRM;
        END;
        GET STACKED DIAGNOSTICS outer_detail = PG_EXCEPTION_DETAIL;
        PERFORM nextval('nested_diagnostic_calls');
        INSERT INTO nested_diagnostic_audit VALUES (p);
        RETURN outer_message || ':' || inner_message || ':' || length(outer_detail)::text;
    END;
END$$;
CREATE FUNCTION nested_diagnostic_parent(n integer) RETURNS text LANGUAGE plpgsql AS $$
DECLARE result text;
BEGIN
    FOR i IN 1..n LOOP
        result := nested_diagnostic_child(i);
    END LOOP;
    RETURN result;
END$$;
SELECT nested_diagnostic_parent(512);
SELECT count(*),sum(id),min(id),max(id) FROM nested_diagnostic_audit;
SELECT nextval('nested_diagnostic_calls');
DROP FUNCTION nested_diagnostic_parent(integer);
DROP FUNCTION nested_diagnostic_child(integer);
DROP TABLE nested_diagnostic_audit;
DROP SEQUENCE nested_diagnostic_calls;
