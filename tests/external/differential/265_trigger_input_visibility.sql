CREATE TABLE trigger_input_rows(id integer PRIMARY KEY);
CREATE TABLE trigger_input_audit(stage text, visible integer, value integer);
CREATE SEQUENCE trigger_input_ticks;
CREATE FUNCTION trigger_input_check(v integer) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$
BEGIN
    INSERT INTO trigger_input_audit VALUES ('check', (SELECT count(*) FROM trigger_input_rows), v);
    RETURN v > 0 AND (SELECT count(*) FROM trigger_input_rows) = 1;
END
$$;
CREATE DOMAIN trigger_input_positive AS integer CHECK(trigger_input_check(VALUE));
CREATE FUNCTION trigger_input_after() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE marker text := repeat('a', 8192);
BEGIN
    PERFORM nextval('trigger_input_ticks');
    INSERT INTO trigger_input_audit VALUES ('before', (SELECT count(*) FROM trigger_input_rows), 0);
    FOR counter IN 1..1 LOOP
        marker := marker || counter::text;
        PERFORM '{1}'::trigger_input_positive[];
        INSERT INTO trigger_input_audit VALUES ('after', (SELECT count(*) FROM trigger_input_rows), length(marker));
    END LOOP;
    RETURN NEW;
END
$$;
CREATE TRIGGER trigger_input_after AFTER INSERT ON trigger_input_rows FOR EACH ROW EXECUTE FUNCTION trigger_input_after();
INSERT INTO trigger_input_rows VALUES (1);
SELECT (SELECT count(*) FROM trigger_input_rows),
       (SELECT count(*) FROM trigger_input_audit),
       (SELECT count(*) FROM trigger_input_audit WHERE stage = 'before'),
       (SELECT count(*) FROM trigger_input_audit WHERE stage = 'check'),
       (SELECT count(*) FROM trigger_input_audit WHERE stage = 'after'),
       (SELECT min(visible) FROM trigger_input_audit),
       (SELECT value FROM trigger_input_audit WHERE stage = 'after'),
       nextval('trigger_input_ticks');
DROP TRIGGER trigger_input_after ON trigger_input_rows;
DROP FUNCTION trigger_input_after();
DROP DOMAIN trigger_input_positive;
DROP FUNCTION trigger_input_check(integer);
DROP TABLE trigger_input_rows;
DROP TABLE trigger_input_audit;
DROP SEQUENCE trigger_input_ticks;
