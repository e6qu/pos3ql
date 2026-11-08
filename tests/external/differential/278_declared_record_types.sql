CREATE DOMAIN record_positive AS integer CHECK(VALUE>0);
CREATE DOMAIN record_label AS text;
CREATE TYPE record_pair AS(id record_positive,label record_label);
CREATE TABLE record_source(id record_positive,label record_label);
INSERT INTO record_source VALUES(7,'seven');
CREATE FUNCTION record_return_positive() RETURNS record_positive LANGUAGE sql
AS $$SELECT 7::record_positive$$;
CREATE FUNCTION record_return_label() RETURNS record_label LANGUAGE sql
AS $$SELECT 'seven'::record_label$$;
CREATE FUNCTION record_return_pair() RETURNS record_pair LANGUAGE sql
AS $$SELECT ROW(7,'seven')::record_pair$$;
CREATE FUNCTION check_declared_record_types() RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE result record; command text;
BEGIN
    FOREACH command IN ARRAY ARRAY[
        'SELECT * FROM record_source',
        'SELECT t.* FROM record_source t',
        'SELECT (t).* FROM record_source t',
        'SELECT f1 AS id,f2 AS label FROM (SELECT (ROW(7::record_positive,''seven''::record_label)).* FROM record_source) q',
        'SELECT record_return_positive() AS id,record_return_label() AS label',
        'SELECT record_return_positive() AS id,record_return_label() AS label FROM record_source',
        'SELECT (record_return_pair()).* FROM record_source',
        'SELECT (record_return_pair()).id AS id,(record_return_pair()).label AS label FROM record_source',
        'UPDATE record_source SET id=id RETURNING *',
        'UPDATE record_source SET id=id RETURNING (record_source).*'
    ] LOOP
        EXECUTE command INTO STRICT result;
        IF pg_typeof(result.id)<>'record_positive'::regtype
            OR pg_typeof(result.label)<>'record_label'::regtype
            OR result.id<>7 OR result.label<>'seven' THEN
            RAISE EXCEPTION 'declared record types changed: %',command;
        END IF;
    END LOOP;
    RETURN true;
END$$;
SELECT check_declared_record_types(),(SELECT count(*) FROM record_source),
    pg_typeof(q.f1),pg_typeof(q.f2),q.f1 IS NULL
FROM (SELECT (ROW(NULL,'text')).* FROM record_source) q;
