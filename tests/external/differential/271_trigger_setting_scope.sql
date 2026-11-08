SET application_name TO 'outer';
SET bytea_output TO 'hex';
SET client_min_messages TO 'notice';
CREATE TABLE trigger_setting_orders(id integer);
CREATE TABLE trigger_setting_audit(
    configured boolean, app text, output text, rendered text, catalog_output text,
    messages text
);
CREATE FUNCTION trigger_setting_configured() RETURNS trigger LANGUAGE plpgsql
SET application_name TO 'configured' AS $$
BEGIN
    PERFORM set_config('bytea_output', 'escape', true);
    PERFORM set_config('client_min_messages', 'warning', false);
    INSERT INTO trigger_setting_audit VALUES (
        true, current_setting('application_name'), current_setting('bytea_output'),
        decode('61', 'hex')::text,
        (SELECT setting FROM pg_catalog.pg_settings WHERE name = 'bytea_output'),
        current_setting('client_min_messages')
    );
    IF TG_LEVEL = 'ROW' THEN RETURN NEW; END IF;
    RETURN NULL;
END
$$;
CREATE FUNCTION trigger_setting_observer() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO trigger_setting_audit VALUES (
        false, current_setting('application_name'), current_setting('bytea_output'),
        decode('61', 'hex')::text,
        (SELECT setting FROM pg_catalog.pg_settings WHERE name = 'bytea_output'),
        current_setting('client_min_messages')
    );
    IF TG_LEVEL = 'ROW' THEN RETURN NEW; END IF;
    RETURN NULL;
END
$$;
CREATE TRIGGER a_before BEFORE INSERT ON trigger_setting_orders
FOR EACH ROW EXECUTE FUNCTION trigger_setting_configured();
CREATE TRIGGER b_before BEFORE INSERT ON trigger_setting_orders
FOR EACH ROW EXECUTE FUNCTION trigger_setting_observer();
CREATE TRIGGER c_after AFTER INSERT ON trigger_setting_orders
FOR EACH ROW EXECUTE FUNCTION trigger_setting_configured();
CREATE TRIGGER d_after AFTER INSERT ON trigger_setting_orders
FOR EACH ROW EXECUTE FUNCTION trigger_setting_observer();
CREATE TRIGGER e_statement AFTER INSERT ON trigger_setting_orders
FOR EACH STATEMENT EXECUTE FUNCTION trigger_setting_configured();
CREATE TRIGGER f_statement AFTER INSERT ON trigger_setting_orders
FOR EACH STATEMENT EXECUTE FUNCTION trigger_setting_observer();
INSERT INTO trigger_setting_orders VALUES (1), (2);
SELECT count(*),
    sum(CASE WHEN configured THEN 1 ELSE 0 END),
    bool_and(app = CASE WHEN configured THEN 'configured' ELSE 'outer' END),
    bool_and(output = CASE WHEN configured THEN 'escape' ELSE 'hex' END),
    bool_and(rendered = CASE WHEN configured THEN 'a' ELSE '\x61' END),
    bool_and(catalog_output = output),
    bool_and(messages = 'warning'),
    current_setting('application_name'), current_setting('bytea_output'),
    current_setting('client_min_messages')
FROM trigger_setting_audit;
DROP TABLE trigger_setting_orders;
DROP TABLE trigger_setting_audit;
DROP FUNCTION trigger_setting_configured();
DROP FUNCTION trigger_setting_observer();
RESET application_name;
RESET bytea_output;
RESET client_min_messages;
