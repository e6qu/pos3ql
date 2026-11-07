CREATE TABLE loop_frame_result(total integer, outer_name text, caught integer, iterated boolean);
DO $$DECLARE i text := 'outside'; total integer := 0; caught integer := 0; BEGIN
    FOR i IN 1..16384 LOOP
        BEGIN
            FOR i IN 1..2 LOOP
                total := total + 1;
                IF total=1 THEN RAISE division_by_zero; END IF;
            END LOOP;
        EXCEPTION WHEN division_by_zero THEN
            caught := caught + 1;
        END;
    END LOOP;
    INSERT INTO loop_frame_result VALUES(total,i,caught,FOUND);
END$$;
SELECT total,outer_name,caught,iterated FROM loop_frame_result;
DROP TABLE loop_frame_result;
CREATE FUNCTION loop_frame_outputs(OUT i text, OUT total integer) LANGUAGE plpgsql AS $$BEGIN
    i := 'outside'; total := 0;
    <<outer_loop>>
    FOR i IN 1..5 LOOP
        FOR j IN 1..3 LOOP
            total := total + 1;
            CONTINUE outer_loop WHEN j=1 AND i<3;
            EXIT outer_loop WHEN total=5;
        END LOOP;
    END LOOP;
END$$;
SELECT i,total FROM loop_frame_outputs();
DROP FUNCTION loop_frame_outputs();
CREATE FUNCTION loop_frame_return() RETURNS text LANGUAGE plpgsql AS $$BEGIN
    FOR i IN 1..2 LOOP
        FOR j IN 1..2 LOOP
            RETURN 'returned:'||i||':'||j;
        END LOOP;
    END LOOP;
    RETURN 'unreachable';
END$$;
SELECT loop_frame_return();
DROP FUNCTION loop_frame_return();
