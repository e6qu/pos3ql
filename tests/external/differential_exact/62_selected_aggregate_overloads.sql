CREATE FUNCTION public.sum(value integer) RETURNS integer LANGUAGE sql AS 'SELECT $1+100';
CREATE TABLE aggregate_choice(v integer);
INSERT INTO aggregate_choice VALUES(1),(2);
CREATE SEQUENCE aggregate_effects;
SET search_path=public,pg_catalog;
SELECT sum(v) FROM aggregate_choice ORDER BY v;
SELECT pg_typeof(sum(v)) FROM aggregate_choice ORDER BY v;
SELECT sum(v),count(*) FROM aggregate_choice GROUP BY v ORDER BY v;
SELECT sum(1);
SELECT sum(v) FROM aggregate_choice ORDER BY v FOR UPDATE;
SELECT sum(v),row_number() OVER(ORDER BY v) FROM aggregate_choice ORDER BY v;
SELECT sum(1) FROM aggregate_choice HAVING true;
SELECT sum(1) FROM aggregate_choice HAVING false;
SELECT sum(1) FROM aggregate_choice WHERE false HAVING true;
SELECT result.value FROM (SELECT sum(1) AS value FROM aggregate_choice HAVING true) result;
SELECT sum(nextval('aggregate_effects')::integer) FROM aggregate_choice HAVING false;

-- next request
SET search_path=public,pg_catalog;
SELECT sum(v),count(*) FROM aggregate_choice;
-- next request
SET search_path=public,pg_catalog;
SELECT sum(v) FROM aggregate_choice HAVING true;
-- next request
SET search_path=public,pg_catalog;
SELECT sum(DISTINCT nextval('aggregate_effects')::integer);
-- next request
SET search_path=public,pg_catalog;
SELECT sum(nextval('aggregate_effects')::integer) FILTER(WHERE true);
-- next request
SET search_path=public,pg_catalog;
SELECT sum(nextval('aggregate_effects')::integer) OVER();
-- next request
SELECT row_number();
-- next request
SELECT nextval('aggregate_effects');
SET search_path=pg_catalog,public;
SELECT sum(v),pg_typeof(sum(v)) FROM aggregate_choice;
DROP TABLE aggregate_choice;
DROP FUNCTION public.sum(integer);
DROP SEQUENCE aggregate_effects;
RESET search_path;
