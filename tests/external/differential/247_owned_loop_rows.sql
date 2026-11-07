CREATE TABLE owned_loop_results(kind text, value text, total integer, iterated boolean);
CREATE SEQUENCE owned_loop_calls;
CREATE FUNCTION owned_loop_row(v integer, OUT n integer, OUT label text) RETURNS record
LANGUAGE sql VOLATILE AS $$SELECT v,'call:'||nextval('owned_loop_calls')$$;
DO $$DECLARE r record; total integer := 0; label text; remembered text; item integer := 42; BEGIN
    FOR r IN SELECT input.n,input.name AS label FROM (VALUES(1,'one'),(2,'two')) AS input(n,name) LOOP
        total := total + r.n;
        label := r.label;
    END LOOP;
    INSERT INTO owned_loop_results VALUES('static',label,total,FOUND);
    FOR r IN EXECUTE 'SELECT 4 AS n,$1::text AS label' USING 'four' LOOP
        total := total + r.n;
        label := r.label;
    END LOOP;
    INSERT INTO owned_loop_results VALUES('dynamic',label,total,FOUND);
    remembered := r.label;
    FOR r IN SELECT 99 AS n,'unused'::text AS label WHERE false LOOP
        RAISE EXCEPTION 'empty query body executed';
    END LOOP;
    INSERT INTO owned_loop_results VALUES('empty',coalesce(r.label,'<null>'),total,FOUND);
    FOR r IN SELECT source.n,source.label FROM owned_loop_row(5) AS source LOOP
        total := total + r.n;
        label := r.label;
    END LOOP;
    INSERT INTO owned_loop_results VALUES('function',label,total,FOUND);
    FOREACH item IN ARRAY ARRAY[]::integer[] LOOP
        RAISE EXCEPTION 'empty array body executed';
    END LOOP;
    INSERT INTO owned_loop_results VALUES('array_empty',item::text,total,FOUND);
    INSERT INTO owned_loop_results VALUES('saved',remembered,total,FOUND);
END$$;
SELECT kind,value,total,iterated FROM owned_loop_results ORDER BY kind;
SELECT currval('owned_loop_calls');
DROP FUNCTION owned_loop_row(integer);
DROP SEQUENCE owned_loop_calls;
DROP TABLE owned_loop_results;
