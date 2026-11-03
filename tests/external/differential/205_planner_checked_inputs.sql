-- PostgreSQL 18.6 clauses.c simplifies conditions before visiting dead results.
-- Direct type-input literals still belong to parse analysis, before planning.
CREATE SEQUENCE planner_checks;
CREATE TYPE planner_choice AS ENUM('a');
CREATE DOMAIN planner_checked AS planner_choice CHECK(nextval('planner_checks')>0);
CREATE FUNCTION planner_array() RETURNS planner_checked[] LANGUAGE sql AS 'SELECT ''{a}''::planner_checked[]';
CREATE FUNCTION planner_scalar() RETURNS integer LANGUAGE sql AS 'SELECT cardinality(''{a}''::planner_checked[])';
ALTER SEQUENCE planner_checks RESTART WITH 1;
BEGIN;
DECLARE planner_not NO SCROLL CURSOR FOR SELECT CASE WHEN NOT true THEN planner_array() ELSE NULL END;
DECLARE planner_comparison NO SCROLL CURSOR FOR SELECT CASE WHEN 1=2 THEN planner_array() ELSE NULL END;
DECLARE planner_simple_miss NO SCROLL CURSOR FOR SELECT CASE 1 WHEN 2 THEN planner_array() ELSE NULL END;
DECLARE planner_simple_hit NO SCROLL CURSOR FOR SELECT CASE 1 WHEN 1 THEN NULL ELSE planner_array() END;
DECLARE planner_and NO SCROLL CURSOR FOR SELECT false AND cardinality(planner_array())=1;
DECLARE planner_or NO SCROLL CURSOR FOR SELECT true OR cardinality(planner_array())=1;
DECLARE planner_coalesce NO SCROLL CURSOR FOR SELECT coalesce(1+1,planner_scalar());
SELECT 'dead_function_inputs',last_value,is_called FROM planner_checks;
FETCH ALL FROM planner_not;
FETCH ALL FROM planner_comparison;
FETCH ALL FROM planner_simple_miss;
FETCH ALL FROM planner_simple_hit;
FETCH ALL FROM planner_and;
FETCH ALL FROM planner_or;
FETCH ALL FROM planner_coalesce;
SELECT 'still_dead',last_value,is_called FROM planner_checks;
COMMIT;
SELECT CASE WHEN false THEN '{a}'::planner_checked[] ELSE NULL END;
SELECT 'parse_input',last_value,is_called FROM planner_checks;

-- Each rejected inline attempt checks its own body; execution then shares the
-- function's first runtime body across all following statement sites.
CREATE FUNCTION planner_text(integer) RETURNS text LANGUAGE sql AS
$$SELECT format('%s',$1) WHERE cardinality('{a}'::planner_checked[])=1$$;
CREATE CAST(integer AS text) WITH FUNCTION planner_text(integer) AS ASSIGNMENT;
CREATE TABLE planner_values(id integer PRIMARY KEY,value text);
ALTER SEQUENCE planner_checks RESTART WITH 1;
INSERT INTO planner_values VALUES(1,1),(2,2);
SELECT 'values',last_value FROM planner_checks;
INSERT INTO planner_values SELECT value,value FROM generate_series(3,5) source(value);
SELECT 'selected',last_value FROM planner_checks;
UPDATE planner_values SET value=id;
SELECT 'updated',last_value FROM planner_checks;
UPDATE planner_values target SET value=source.value FROM (VALUES(1,11),(2,12)) source(id,value) WHERE target.id=source.id;
SELECT 'joined_update',last_value FROM planner_checks;
INSERT INTO planner_values VALUES(1,'ignored'),(2,'ignored') ON CONFLICT(id) DO UPDATE SET value=excluded.id;
SELECT 'conflict_update',last_value FROM planner_checks;
MERGE INTO planner_values target USING (VALUES(1,21),(6,26)) source(id,value) ON target.id=source.id
WHEN MATCHED THEN UPDATE SET value=source.value
WHEN NOT MATCHED THEN INSERT(id,value) VALUES(source.id,source.value);
SELECT 'merged',last_value FROM planner_checks;
SELECT id,value FROM planner_values ORDER BY id;
DROP TABLE planner_values;
DROP CAST(integer AS text);
CREATE FUNCTION planner_projection(integer) RETURNS text LANGUAGE sql AS
$$SELECT format('%s',$1) || CASE WHEN cardinality('{a}'::planner_checked[])=1 THEN '' ELSE '' END$$;
CREATE CAST(integer AS text) WITH FUNCTION planner_projection(integer) AS ASSIGNMENT;
CREATE TABLE planner_projection_values(id integer PRIMARY KEY,value text);
ALTER SEQUENCE planner_checks RESTART WITH 1;
INSERT INTO planner_projection_values(value,id) SELECT value,id FROM (VALUES(11,1)) source(value,id);
SELECT 'reordered',last_value FROM planner_checks;
INSERT INTO planner_projection_values(value,id) SELECT * FROM (VALUES(22,2)) source(value,id);
SELECT 'wildcard',last_value FROM planner_checks;
INSERT INTO planner_projection_values(value,id) SELECT source.* FROM (VALUES(33,3)) source(value,id);
SELECT 'qualified_wildcard',last_value FROM planner_checks;
INSERT INTO planner_projection_values(value,id) SELECT (source).* FROM (VALUES(44,4)) source(value,id);
SELECT 'record_wildcard',last_value FROM planner_checks;
SELECT id,value FROM planner_projection_values ORDER BY id;
DROP TABLE planner_projection_values;
DROP CAST(integer AS text);
DROP FUNCTION planner_text(integer),planner_scalar(),planner_array(),planner_projection(integer);
DROP DOMAIN planner_checked;
DROP TYPE planner_choice;
DROP SEQUENCE planner_checks;
