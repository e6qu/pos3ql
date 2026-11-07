CREATE TABLE trigger_context_types(id integer, result text);
CREATE FUNCTION trigger_context_type_probe() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE missing text := '';
BEGIN
    BEGIN
        PERFORM TG_TABLESPACE;
    EXCEPTION WHEN undefined_column THEN missing := SQLSTATE; END;
    NEW.result := pg_typeof(TG_NAME)::text || ':' || pg_typeof(TG_RELID)::text
        || ':' || pg_typeof(TG_RELNAME)::text || ':' || pg_typeof(TG_TABLE_NAME)::text
        || ':' || pg_typeof(TG_TABLE_SCHEMA)::text || ':' || pg_typeof(TG_NARGS)::text
        || ':' || pg_typeof(TG_ARGV)::text || ':' || pg_typeof(TG_WHEN)::text
        || ':' || pg_typeof(TG_LEVEL)::text || ':' || pg_typeof(TG_OP)::text
        || ':' || (TG_ARGV IS NULL)::text || ':' || coalesce(TG_ARGV[0],'NULL')
        || ':' || missing;
    RETURN NEW;
END$$;
CREATE TRIGGER context_without_args BEFORE INSERT ON trigger_context_types
    FOR EACH ROW EXECUTE FUNCTION trigger_context_type_probe();
INSERT INTO trigger_context_types(id) VALUES (1);
DROP TRIGGER context_without_args ON trigger_context_types;
CREATE TRIGGER context_with_args BEFORE INSERT ON trigger_context_types
    FOR EACH ROW EXECUTE FUNCTION trigger_context_type_probe('first','second');
INSERT INTO trigger_context_types(id) VALUES (2);
SELECT result FROM trigger_context_types ORDER BY id;
DROP TABLE trigger_context_types;
DROP FUNCTION trigger_context_type_probe();
