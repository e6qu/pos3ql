CREATE TABLE control_frame_results(label text, value text);
CREATE SEQUENCE control_frame_calls;
DO $$DECLARE item integer; counter text := 'outside'; total integer := 0; caught text; BEGIN
    <<outer_loop>>
    FOR counter IN 1..4 LOOP
        BEGIN
            FOR item IN SELECT n FROM (VALUES(1),(2),(3)) AS input(n) LOOP
                CASE item
                    WHEN 1 THEN
                        IF counter=1 THEN CONTINUE outer_loop; END IF;
                    WHEN 2 THEN
                        total := total + 10;
                        IF counter=2 THEN RAISE division_by_zero; END IF;
                    ELSE
                        total := total + 100;
                END CASE;
                total := total + 1;
                EXIT outer_loop WHEN counter=4 AND item=3;
            END LOOP;
        EXCEPTION WHEN division_by_zero THEN
            caught := SQLSTATE;
            BEGIN
                PERFORM nextval('control_frame_calls');
                RAISE check_violation;
            EXCEPTION WHEN check_violation THEN
                INSERT INTO control_frame_results VALUES('inner',SQLSTATE);
            END;
            INSERT INTO control_frame_results VALUES('outer',SQLSTATE);
            CONTINUE outer_loop;
        END;
    END LOOP;
    INSERT INTO control_frame_results VALUES('summary',counter||':'||total||':'||caught||':'||FOUND);
END$$;
DO $$BEGIN
    BEGIN
        BEGIN
            RAISE division_by_zero;
        EXCEPTION WHEN division_by_zero THEN
            BEGIN RAISE check_violation; EXCEPTION WHEN check_violation THEN NULL; END;
            RAISE;
        END;
    EXCEPTION WHEN division_by_zero THEN
        INSERT INTO control_frame_results VALUES('rethrow',SQLSTATE);
    END;
END$$;
SELECT label,value FROM control_frame_results ORDER BY label;
SELECT currval('control_frame_calls');
DROP SEQUENCE control_frame_calls;
DROP TABLE control_frame_results;
CREATE FUNCTION control_frame_arrays(OUT total integer, OUT final_row text) LANGUAGE plpgsql AS $$DECLARE row_values integer[]; item integer; BEGIN
    total := 0;
    FOREACH row_values SLICE 1 IN ARRAY ARRAY[[1,2],[3,4]] LOOP
        FOREACH item IN ARRAY row_values LOOP
            WHILE item>0 LOOP
                LOOP total := total + 1; EXIT; END LOOP;
                item := item - 1;
            END LOOP;
        END LOOP;
    END LOOP;
    final_row := array_to_string(row_values,',');
END$$;
SELECT total,final_row FROM control_frame_arrays();
DROP FUNCTION control_frame_arrays();
CREATE FUNCTION control_frame_return() RETURNS text LANGUAGE plpgsql AS $$BEGIN
    FOR i IN 1..2 LOOP
        BEGIN RETURN 'done:'||i; EXCEPTION WHEN OTHERS THEN RETURN 'caught'; END;
    END LOOP;
    RETURN 'empty';
END$$;
SELECT control_frame_return();
DROP FUNCTION control_frame_return();
