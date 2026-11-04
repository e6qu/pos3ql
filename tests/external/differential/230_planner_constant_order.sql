-- Replays seed 3, statements 3264, 5911 and 6509 from the differential fuzzer.
CREATE SCHEMA planner_order;
SET search_path=planner_order,pg_catalog;
CREATE TABLE fz(id integer,a integer,b integer,s text,flag boolean);
INSERT INTO fz VALUES(1,10,3,'apple',true),(2,-7,0,'banana',false),(3,NULL,5,NULL,NULL),
 (4,2147483647,1,'pear',true),(5,0,-4,'',false),(6,42,NULL,'a_b',true);
SELECT id,min(a),min(flag) FROM fz WHERE ((((999999 / (5 - 0.0)) * (0 * (0.0 + -2.25))) = (7 / ((3 + -2.25) * (0.0 % 2))) AND a < 999999) OR (a < (10.0 % 0.04) OR s NOT LIKE '_pp%')) GROUP BY id ORDER BY 1 NULLS LAST;
SELECT repeat('a_b',2), CASE WHEN '' NOT LIKE 'a%' THEN (-1 / 999999) WHEN 0.0 >= ((-1 * (100 / 0.0)) / 2147483647) THEN a WHEN a IN (7,100,42,1) THEN (((100+a)+-2.25)*((b- -1)/(42+3.14))) END FROM fz;
SELECT CASE WHEN (b IS NULL AND flag) THEN 1 ELSE ((2.0 % -1) * (-7 - (-2147483648 -999999))) END, div(100.0,7.0),s FROM fz WHERE (-2147483648 <= (100 / 0.0) OR (0 % -7.5) >= (((100 -3.14)*(b /1.5))-3.14));
SELECT CASE WHEN true THEN 1 ELSE min(flag) END FROM fz WHERE 1/0>0;
SELECT CASE WHEN false THEN 1/0 ELSE 7 END FROM fz WHERE false;
SELECT CASE WHEN 'A' ILIKE 'a' THEN 7 WHEN 1/0>0 THEN 8 END;
CREATE TABLE escapes(id integer,mark text);
INSERT INTO escapes VALUES(1,'!'),(2,'');
SELECT id,'a_b' LIKE 'a!_b' ESCAPE mark FROM escapes ORDER BY id;
CREATE SEQUENCE escape_calls;
SELECT 'a_b' LIKE 'a!_b' ESCAPE CASE WHEN nextval('escape_calls')>0 THEN '!' END FROM escapes ORDER BY id;
SELECT last_value FROM escape_calls;
SELECT 'a_b' LIKE 'a!_b' ESCAPE (SELECT '!');
SELECT CASE WHEN 'a_b' LIKE 'a!_b' ESCAPE (SELECT '!') THEN 9 ELSE 1 END;
DROP SCHEMA planner_order CASCADE;
RESET search_path;
