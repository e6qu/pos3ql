CREATE SEQUENCE from_call_numbers;
CREATE FUNCTION from_record(input integer, OUT n integer, OUT label text)
LANGUAGE sql AS $$SELECT input,'row:'||input FROM generate_series(1,2)$$;
CREATE FUNCTION from_procedure(INOUT n integer, OUT label text)
LANGUAGE plpgsql AS $$BEGIN
    n := n+1;
    label := 'call:'||nextval('from_call_numbers');
END$$;
SELECT n,label FROM from_record(2);
SELECT n,label FROM from_procedure(7);
SELECT renamed.number,renamed.description,renamed.position
FROM from_procedure(8) WITH ORDINALITY AS renamed(number,description,position);
SELECT * FROM ROWS FROM(from_record(3),from_procedure(9)) WITH ORDINALITY;
SELECT source.n,result.n,result.label
FROM (VALUES(2),(4)) AS source(n),LATERAL from_record(source.n) AS result
ORDER BY source.n;
CREATE FUNCTION from_strict(input integer, effect bigint, OUT n integer, OUT label text)
LANGUAGE sql STRICT AS $$SELECT input,'ran'::text$$;
SELECT n,label FROM from_strict(NULL,nextval('from_call_numbers'));
SELECT last_value FROM from_call_numbers;
CREATE FUNCTION from_single(INOUT value integer)
LANGUAGE sql AS $$SELECT value+1$$;
SELECT value FROM from_single(10);
CREATE FUNCTION from_single_set(INOUT value integer) RETURNS SETOF integer
LANGUAGE sql AS $$SELECT value+number FROM generate_series(1,2) AS series(number)$$;
SELECT value FROM from_single_set(10) ORDER BY value;
SELECT count(*) FROM from_single_set(NULL);
CREATE FUNCTION from_empty(OUT n integer, OUT label text)
LANGUAGE sql AS $$SELECT 1,'empty'::text WHERE false$$;
SELECT n,label FROM from_empty();
CREATE TYPE from_pair AS (n integer,label text);
CREATE FUNCTION from_named(input integer) RETURNS from_pair
LANGUAGE sql STRICT AS $$SELECT input,'named:'||input$$;
SELECT n,label FROM from_named(12);
SELECT n,label FROM from_named(NULL);
CREATE FUNCTION from_scalar(input integer) RETURNS integer
LANGUAGE sql AS $$SELECT input+number FROM generate_series(1,2) AS series(number)$$;
SELECT * FROM from_scalar(20);
SELECT * FROM from_scalar(NULL);
DROP FUNCTION from_scalar(integer);
DROP FUNCTION from_named(integer);
DROP TYPE from_pair;
DROP FUNCTION from_empty();
DROP FUNCTION from_single_set(integer);
DROP FUNCTION from_single(integer);
DROP FUNCTION from_strict(integer,bigint);
DROP FUNCTION from_procedure(integer);
DROP FUNCTION from_record(integer);
DROP SEQUENCE from_call_numbers;
