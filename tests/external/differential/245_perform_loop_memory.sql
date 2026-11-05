CREATE SEQUENCE perform_loop_calls;
CREATE TABLE perform_loop_result(seen integer,total integer,calls bigint);
CREATE FUNCTION perform_loop_text() RETURNS text LANGUAGE plpgsql
AS $$BEGIN RETURN repeat('x',4096); END$$;
DO $$DECLARE seen integer := 0; total integer := 0; count integer; BEGIN
    FOR i IN 1..4096 LOOP
        PERFORM i WHERE i%2=0;
        IF FOUND THEN seen := seen+1; END IF;
        GET DIAGNOSTICS count=ROW_COUNT;
        total := total+count;
        PERFORM perform_loop_text();
        IF i%128=0 THEN PERFORM nextval('perform_loop_calls'); END IF;
    END LOOP;
    INSERT INTO perform_loop_result VALUES(seen,total,currval('perform_loop_calls'));
END$$;
SELECT seen,total,calls FROM perform_loop_result;
DROP TABLE perform_loop_result;
DROP FUNCTION perform_loop_text();
DROP SEQUENCE perform_loop_calls;
