CREATE TABLE selection_parent(id integer PRIMARY KEY, value integer);
CREATE TABLE selection_child(id integer PRIMARY KEY, value integer);
CREATE TABLE selection_audit(stage text, observed integer);
CREATE SEQUENCE selection_ticks;
INSERT INTO selection_parent VALUES (1,10),(2,20),(3,30);
INSERT INTO selection_child VALUES (1,10),(2,20),(3,30);
CREATE FUNCTION selection_before() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE marker text := repeat('p',8192);
BEGIN
    PERFORM nextval('selection_ticks');
    INSERT INTO selection_audit VALUES ('before', (SELECT count(*) FROM selection_parent WHERE value = id*10+1));
    UPDATE selection_child SET value = value+1 WHERE id <= NEW.id;
    EXECUTE 'UPDATE selection_child SET value=value+10 WHERE id=$1' USING NEW.id;
    BEGIN
        DELETE FROM selection_child WHERE id = NEW.id;
        RAISE EXCEPTION 'undo the nested delete';
    EXCEPTION WHEN raise_exception THEN
        INSERT INTO selection_audit VALUES ('caught', (SELECT count(*) FROM selection_child));
    END;
    INSERT INTO selection_child VALUES (NEW.id,1000)
        ON CONFLICT(id) DO UPDATE SET value=selection_child.value+100;
    DELETE FROM selection_child WHERE id > 99;
    EXECUTE 'DELETE FROM selection_child WHERE id>$1' USING 99;
    FOR counter IN 1..1 LOOP
        marker := marker || counter::text;
    END LOOP;
    INSERT INTO selection_audit VALUES ('after',length(marker));
    RETURN NEW;
END
$$;
CREATE FUNCTION selection_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO selection_audit VALUES ('statement', (SELECT count(*) FROM selection_parent WHERE value = id*10+1));
    RETURN NULL;
END
$$;
CREATE TRIGGER selection_before BEFORE UPDATE ON selection_parent FOR EACH ROW EXECUTE FUNCTION selection_before();
CREATE TRIGGER selection_statement AFTER UPDATE ON selection_parent FOR EACH STATEMENT EXECUTE FUNCTION selection_statement();
UPDATE selection_parent SET value=value+1 WHERE id < 4;
SELECT (SELECT sum(value) FROM selection_parent),
       (SELECT sum(value) FROM selection_child),
       (SELECT min(value) FROM selection_child),
       (SELECT max(value) FROM selection_child),
       (SELECT count(*) FROM selection_audit),
       (SELECT min(observed) FROM selection_audit WHERE stage='before'),
       (SELECT max(observed) FROM selection_audit WHERE stage='before'),
       (SELECT min(observed) FROM selection_audit WHERE stage='caught'),
       (SELECT min(observed) FROM selection_audit WHERE stage='after'),
       (SELECT observed FROM selection_audit WHERE stage='statement'),
       nextval('selection_ticks');
DROP TRIGGER selection_before ON selection_parent;
DROP TRIGGER selection_statement ON selection_parent;
DROP FUNCTION selection_before();
DROP FUNCTION selection_statement();
DROP TABLE selection_parent;
DROP TABLE selection_child;
DROP TABLE selection_audit;
DROP SEQUENCE selection_ticks;
