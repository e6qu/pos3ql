-- PREPARE runs parse analysis, which converts each literal once: a literal's
-- input notice or error is raised at PREPARE, and EXECUTE reads the
-- converted value without converting it again. Planning errors, such as
-- division by zero or a length coercion, wait for EXECUTE.
CREATE TABLE c540 (q tsquery, n int, v varchar(3));
PREPARE c540_cast AS SELECT ''::tsquery AS q, int4 '12' + 1 AS n, '{1,2}'::int[] AS a;
EXECUTE c540_cast;
EXECUTE c540_cast;
PREPARE c540_insert AS INSERT INTO c540 VALUES ('', 1, 'ab');
EXECUTE c540_insert;
EXECUTE c540_insert;
PREPARE c540_compare AS SELECT n, q = '' AS empty FROM c540 ORDER BY n;
EXECUTE c540_compare;
EXECUTE c540_compare;
PREPARE c540_update AS UPDATE c540 SET q = '' WHERE n = 1;
EXECUTE c540_update;
PREPARE c540_bad AS SELECT 'x'::int;
PREPARE c540_bad_compare AS SELECT * FROM c540 WHERE n = 'q';
PREPARE c540_bad_insert AS INSERT INTO c540 (n) VALUES ('y');
PREPARE c540_length AS SELECT '123.45'::numeric(3,1);
EXECUTE c540_length;
PREPARE c540_divide AS SELECT 1/0;
EXECUTE c540_divide;
PREPARE c540_modifier AS SELECT 'abcd'::varchar(2) AS v, '1 day 2 hours'::interval hour AS i;
EXECUTE c540_modifier;
PREPARE c540_now AS SELECT 'now'::timestamptz = 'now'::timestamptz AS same;
EXECUTE c540_now;
PREPARE c540_param AS SELECT ''::tsquery AS q, $1::int + 1 AS n;
EXECUTE c540_param(1);
EXECUTE c540_param(2);
DEALLOCATE ALL;
