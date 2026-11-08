CREATE DOMAIN record_positive AS integer CHECK(VALUE>0);
CREATE DOMAIN record_label AS text;
CREATE TYPE record_pair AS(id record_positive,label record_label);
CREATE TABLE record_source(id record_positive,label record_label);
CREATE TABLE record_field_copies(id record_positive,label record_label);
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
        'SELECT id,label FROM record_source',
        'SELECT t.id,t.label FROM record_source t',
        'SELECT public.record_source.id,public.record_source.label FROM public.record_source',
        'SELECT t.* FROM record_source t',
        'SELECT (t).* FROM record_source t',
        'SELECT f1 AS id,f2 AS label FROM (SELECT (ROW(7::record_positive,''seven''::record_label)).* FROM record_source) q',
        'SELECT record_return_positive() AS id,record_return_label() AS label',
        'SELECT record_return_positive() AS id,record_return_label() AS label FROM record_source',
        'SELECT (ARRAY[7::record_positive])[1] AS id,(ARRAY[''seven''::record_label])[1] AS label',
        'SELECT (ROW(7,''seven'')::record_pair).*',
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
CREATE FUNCTION check_unknown_record_output() RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE result record; code text; message text;
BEGIN
    BEGIN
        EXECUTE 'SELECT (ROW(NULL,''text'')).*' INTO result;
        RAISE EXCEPTION 'unknown record expansion was accepted';
    EXCEPTION WHEN SQLSTATE 'XX000' THEN
        GET STACKED DIAGNOSTICS code=RETURNED_SQLSTATE,message=MESSAGE_TEXT;
        IF message<>'failed to find conversion function from unknown to text' THEN
            RAISE EXCEPTION 'unknown record expansion returned the wrong error: %',message;
        END IF;
    END;
    EXECUTE 'SELECT (ROW(NULL,''text''::text)).f2 AS value' INTO result;
    RETURN code='XX000' AND result.value='text';
END$$;
CREATE FUNCTION check_null_record_fields() RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE result record; copied record; matched boolean;
BEGIN
    EXECUTE 'SELECT NULL::record_positive AS id,NULL::record_label AS label' INTO result;
    SELECT result.id,result.label INTO copied;
    INSERT INTO record_field_copies SELECT result.id,result.label;
    UPDATE record_field_copies SET id=result.id,label=result.label;
    EXECUTE 'SELECT 7::record_positive AS id,''seven''::record_label AS label' INTO result;
    SELECT result.label=result.label INTO matched;
    RETURN matched AND copied.id IS NULL AND copied.label IS NULL
        AND pg_typeof(copied.id)='record_positive'::regtype
        AND pg_typeof(copied.label)='record_label'::regtype;
END$$;
CREATE FUNCTION check_following_record_fields() RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE result record;
BEGIN
    EXECUTE 'SELECT (ROW(7,''seven'')::record_pair).*,(SELECT NULL::record_positive) AS tail,((ARRAY(SELECT ROW(7,''seven'')::record_pair))[1]).id AS array_id' INTO result;
    RETURN result.id=7 AND result.label='seven' AND result.tail IS NULL
        AND result.array_id=7
        AND pg_typeof(result.id)='record_positive'::regtype
        AND pg_typeof(result.label)='record_label'::regtype
        AND pg_typeof(result.tail)='record_positive'::regtype
        AND pg_typeof(result.array_id)='record_positive'::regtype;
END$$;
SELECT check_declared_record_types(),(SELECT count(*) FROM record_source),
    pg_typeof(q.f1),pg_typeof(q.f2),
    q.f1 IS NULL AND check_unknown_record_output() AND check_null_record_fields()
        AND check_following_record_fields()
FROM (SELECT (ROW(NULL::text,'text'::text)).* FROM record_source) q;
