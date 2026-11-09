CREATE TABLE transition_write_driver(id integer PRIMARY KEY,delta integer);
CREATE TABLE transition_write_target(id integer PRIMARY KEY,value integer);
CREATE FUNCTION transition_write_program() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE transition_write_target
    SET value=transition_write_target.value+changed_rows.delta
    FROM changed_rows WHERE transition_write_target.id=changed_rows.id;
    DELETE FROM transition_write_target USING changed_rows
    WHERE transition_write_target.id=changed_rows.id AND changed_rows.delta=9;
    RETURN NULL;
END$$;
CREATE TRIGGER transition_write_after AFTER UPDATE ON transition_write_driver
REFERENCING NEW TABLE AS changed_rows FOR EACH STATEMENT EXECUTE FUNCTION transition_write_program();
INSERT INTO transition_write_driver VALUES(1,3),(2,9);
INSERT INTO transition_write_target VALUES(1,10),(2,20);
UPDATE transition_write_driver SET delta=delta;
SELECT id,value FROM transition_write_target;
