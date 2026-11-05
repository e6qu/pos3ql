CREATE TABLE application_codes_audit(code text, handler text);
DO $$DECLARE requested_code text; BEGIN
    FOREACH requested_code IN ARRAY ARRAY['PZ001','PZ002','PZ003','PZ004','PZ999'] LOOP
        BEGIN
            RAISE EXCEPTION USING ERRCODE=requested_code, MESSAGE='application error';
        EXCEPTION WHEN OTHERS THEN
            INSERT INTO application_codes_audit VALUES(SQLSTATE,'others');
        END;
        BEGIN
            RAISE EXCEPTION USING ERRCODE=requested_code, MESSAGE='application error';
        EXCEPTION WHEN SQLSTATE 'PZ000' THEN
            INSERT INTO application_codes_audit VALUES(SQLSTATE,'class');
        END;
    END LOOP;
END$$;
DO $$BEGIN
    BEGIN RAISE SQLSTATE 'PZ001'; EXCEPTION WHEN SQLSTATE 'PZ001' THEN
        INSERT INTO application_codes_audit VALUES(SQLSTATE,'exact'); END;
    BEGIN RAISE SQLSTATE 'PZ002'; EXCEPTION WHEN SQLSTATE 'PZ002' THEN
        INSERT INTO application_codes_audit VALUES(SQLSTATE,'exact'); END;
    BEGIN RAISE SQLSTATE 'PZ003'; EXCEPTION WHEN SQLSTATE 'PZ003' THEN
        INSERT INTO application_codes_audit VALUES(SQLSTATE,'exact'); END;
    BEGIN RAISE SQLSTATE 'PZ004'; EXCEPTION WHEN SQLSTATE 'PZ004' THEN
        INSERT INTO application_codes_audit VALUES(SQLSTATE,'exact'); END;
    BEGIN RAISE SQLSTATE 'PZ999'; EXCEPTION WHEN SQLSTATE 'PZ999' THEN
        INSERT INTO application_codes_audit VALUES(SQLSTATE,'exact'); END;
END$$;
SELECT code||':'||handler FROM application_codes_audit ORDER BY code,handler;
CREATE FUNCTION application_code_rows() RETURNS SETOF integer LANGUAGE plpgsql AS $$BEGIN
    RETURN NEXT 1;
    BEGIN RAISE SQLSTATE 'PZ004'; EXCEPTION WHEN OTHERS THEN RETURN NEXT 2; END;
END$$;
SELECT * FROM application_code_rows();
DROP FUNCTION application_code_rows();
DROP TABLE application_codes_audit;
