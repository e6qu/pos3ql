CREATE FUNCTION handler_identifier_case() RETURNS text LANGUAGE plpgsql AS $$
DECLARE result text := '';
BEGIN
    BEGIN
        RAISE EXCEPTION 'caught';
    EXCEPTION WHEN OTHERS THEN
        BEGIN
            result := SQLERRM || "SQLERRM";
        EXCEPTION WHEN undefined_column THEN result := 'error-case'; END;
    END;
    BEGIN
        result := result || FOUND::text || "FOUND"::text;
    EXCEPTION WHEN undefined_column THEN result := result || ':status-case'; END;
    RETURN result;
END$$;
SELECT handler_identifier_case();
DROP FUNCTION handler_identifier_case();
CREATE FUNCTION local_identifier_case() RETURNS text LANGUAGE plpgsql AS $$
DECLARE MixedCase text := 'lower'; "MixedCase" text := 'quoted';
    "spaced name" text := 'space';
BEGIN
    RETURN mixedcase || ':' || "MixedCase" || ':' || "spaced name";
END$$;
SELECT local_identifier_case();
DROP FUNCTION local_identifier_case();
CREATE FUNCTION loop_identifier_case() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE Total integer := 0;
BEGIN
    <<Work>> FOR I IN 1..3 LOOP
        TOTAL := Total + i;
        EXIT work WHEN total >= 3;
    END LOOP;
    RETURN total;
END$$;
SELECT loop_identifier_case();
DROP FUNCTION loop_identifier_case();
CREATE TABLE context_identifier_rows(value text);
CREATE FUNCTION context_identifier_case() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.value := TG_NAME;
    BEGIN
        NEW.value := NEW.value || "TG_NAME";
    EXCEPTION WHEN undefined_column THEN NEW.value := NEW.value || ':context-case'; END;
    RETURN NEW;
END$$;
CREATE TRIGGER case_trigger BEFORE INSERT ON context_identifier_rows
    FOR EACH ROW EXECUTE FUNCTION context_identifier_case();
INSERT INTO context_identifier_rows VALUES ('');
SELECT value FROM context_identifier_rows;
DROP TABLE context_identifier_rows;
DROP FUNCTION context_identifier_case();
CREATE FUNCTION record_identifier_case() RETURNS text LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
    SELECT 'lower' AS mixed,'quoted' AS "Mixed" INTO r;
    RETURN r.mixed || ':' || r."Mixed";
END$$;
SELECT record_identifier_case();
DROP FUNCTION record_identifier_case();
