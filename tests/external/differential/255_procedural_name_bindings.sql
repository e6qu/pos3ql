CREATE FUNCTION quoted_cursor_names() RETURNS text LANGUAGE plpgsql AS $$
DECLARE "cursor "" name" refcursor; "fetched value" text; result text;
BEGIN
    OPEN "cursor "" name" FOR SELECT 'first';
    FETCH NEXT FROM "cursor "" name" INTO "fetched value";
    result := "fetched value";
    CLOSE "cursor "" name";
    OPEN "cursor "" name" FOR SELECT 'skip' UNION ALL SELECT 'second';
    MOVE NEXT FROM "cursor "" name";
    FETCH NEXT FROM "cursor "" name" INTO "fetched value";
    CLOSE "cursor "" name";
    RETURN result || ':' || "fetched value";
END$$;
SELECT quoted_cursor_names();
DROP FUNCTION quoted_cursor_names();
CREATE FUNCTION cursor_shadowed_variables() RETURNS text LANGUAGE plpgsql AS $$
DECLARE "cursor name" refcursor; fetched text; result text; FOUND integer := 42;
    SQLSTATE integer := 7; SQLERRM integer := 8;
BEGIN
    result := SQLSTATE::text || ':' || pg_typeof(SQLSTATE)::text;
    BEGIN
        RAISE EXCEPTION 'caught';
    EXCEPTION WHEN OTHERS THEN
        OPEN "cursor name" FOR SELECT FOUND::text || ':' || pg_typeof(FOUND)::text
            || ':' || SQLSTATE || ':' || SQLERRM;
        FETCH NEXT FROM "cursor name" INTO fetched;
        CLOSE "cursor name";
    END;
    RETURN result || ':' || fetched;
END$$;
SELECT cursor_shadowed_variables();
DROP FUNCTION cursor_shadowed_variables();
CREATE TABLE trigger_name_edges_audit(value text PRIMARY KEY);
INSERT INTO trigger_name_edges_audit VALUES ('42P01');
CREATE TABLE trigger_name_edges(id integer, result text);
CREATE FUNCTION trigger_name_edges_probe() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE TG_NAME text := 'local'; FOUND integer := 42; SQLSTATE text := 'outer state';
    SQLERRM text := 'outer message'; result text;
BEGIN
    result := TG_NAME || ':' || FOUND::text || ':' || pg_typeof(FOUND)::text
        || ':' || SQLSTATE || ':' || SQLERRM;
    BEGIN
        PERFORM "NEW".id;
    EXCEPTION WHEN OTHERS THEN
        result := result || ':' || SQLSTATE;
        INSERT INTO trigger_name_edges_audit VALUES ('42P01') ON CONFLICT(value)
            DO UPDATE SET value = SQLSTATE || ':' || pg_typeof(SQLSTATE)::text || ':' || FOUND::text;
    END;
    NEW.result := result;
    RETURN NEW;
END$$;
CREATE TRIGGER name_edges BEFORE INSERT ON trigger_name_edges
    FOR EACH ROW EXECUTE FUNCTION trigger_name_edges_probe();
INSERT INTO trigger_name_edges(id) VALUES (1);
SELECT result FROM trigger_name_edges;
SELECT value FROM trigger_name_edges_audit;
DROP TABLE trigger_name_edges_audit;
DROP TABLE trigger_name_edges;
DROP FUNCTION trigger_name_edges_probe();
