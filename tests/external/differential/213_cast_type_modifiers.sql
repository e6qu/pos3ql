-- Cast functions receive the destination modifier and explicit/assignment flag.
CREATE FUNCTION target_varchar(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS
  $$SELECT (CASE WHEN $2=9 THEN CASE WHEN $3 THEN 'e9' ELSE 'a9' END ELSE 'none' END)::varchar$$;
CREATE CAST(integer AS varchar) WITH FUNCTION target_varchar(integer,integer,boolean) AS ASSIGNMENT;
CREATE DOMAIN target_short AS varchar(5);
CREATE TABLE target_modifiers(value varchar(5),items varchar(5)[],domain_value target_short);
SELECT 7::varchar(5);
SELECT 7::varchar;
SELECT ARRAY[7,8]::varchar(5)[];
SELECT (ARRAY[7,8]::integer[])::varchar(5)[];
SELECT ARRAY[ARRAY[7],ARRAY[8]]::varchar(5)[];
SELECT 7::target_short;
SELECT ARRAY[7,8]::target_short[];
INSERT INTO target_modifiers VALUES(7,ARRAY[7,8],7);
SELECT value,items,domain_value FROM target_modifiers;
DROP TABLE target_modifiers;
DROP DOMAIN target_short;
DROP CAST(integer AS varchar);
DROP FUNCTION target_varchar(integer,integer,boolean);
