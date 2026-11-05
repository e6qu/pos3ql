CREATE FUNCTION handler_assignment_inputs() RETURNS text LANGUAGE plpgsql AS $$
DECLARE document jsonb := '{}';
BEGIN
    BEGIN
        RAISE EXCEPTION 'caught';
    EXCEPTION WHEN OTHERS THEN
        document[SQLERRM] := to_jsonb(SQLSTATE);
    END;
    RETURN document->>'caught';
END$$;
SELECT handler_assignment_inputs();
DROP FUNCTION handler_assignment_inputs();
CREATE TABLE handler_assignment_rows(document jsonb);
CREATE FUNCTION handler_row_assignment() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    BEGIN
        RAISE EXCEPTION 'caught';
    EXCEPTION WHEN OTHERS THEN
        NEW.document[SQLERRM] := to_jsonb(SQLSTATE);
    END;
    RETURN NEW;
END$$;
CREATE TRIGGER handler_assignment_before BEFORE INSERT ON handler_assignment_rows
    FOR EACH ROW EXECUTE FUNCTION handler_row_assignment();
INSERT INTO handler_assignment_rows VALUES ('{}');
SELECT document->>'caught' FROM handler_assignment_rows;
DROP TABLE handler_assignment_rows;
DROP FUNCTION handler_row_assignment();
