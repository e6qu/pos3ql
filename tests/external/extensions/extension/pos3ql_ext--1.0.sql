CREATE TABLE extension_rows (
    id integer PRIMARY KEY,
    value text NOT NULL
);
CREATE TABLE extension_config (
    key text PRIMARY KEY,
    built_in boolean NOT NULL
);
SELECT pg_catalog.pg_extension_config_dump(
    'extension_config',
    'WHERE NOT built_in'
);
CREATE SEQUENCE extension_sequence;
CREATE FUNCTION extension_identity(value text)
RETURNS text LANGUAGE SQL AS $$ SELECT value $$;
CREATE VIEW extension_view AS
SELECT id, value FROM extension_rows;
CREATE MATERIALIZED VIEW extension_snapshot AS
SELECT 42 AS value;
CREATE FOREIGN DATA WRAPPER extension_member_wrapper NO HANDLER NO VALIDATOR;
CREATE SERVER extension_member_server FOREIGN DATA WRAPPER extension_member_wrapper;
CREATE FOREIGN TABLE extension_member_foreign (id integer)
SERVER extension_member_server;
CREATE FUNCTION extension_member_event_function()
RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN RETURN; END $$;
CREATE EVENT TRIGGER extension_member_event ON ddl_command_end
EXECUTE FUNCTION extension_member_event_function();
