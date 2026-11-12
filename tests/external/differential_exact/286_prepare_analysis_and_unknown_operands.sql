-- PREPARE analyses its statement: constant inputs and FROM-less WHERE types
-- fail there, domain parameters are listed and checked as the domain, and an
-- untyped operand of a built-in operator follows PostgreSQL's candidate
-- selection, at PREPARE and when a query runs.
CREATE DOMAIN pos286 AS int CHECK (VALUE > 0);
CREATE TYPE mood286 AS ENUM ('sad', 'ok');
CREATE TYPE pair286 AS (a int, b text);
PREPARE c1_286 AS SELECT 'a'::int;
PREPARE c2_286 AS SELECT 1 WHERE 'x';
PREPARE c3_286 AS SELECT 1 WHERE 1;
PREPARE c4_286 AS SELECT 1 WHERE $1 + 1;
PREPARE d1_286 AS SELECT $1::pos286;
PREPARE d2_286(pos286) AS SELECT $1;
PREPARE d3_286(pos286[], text) AS SELECT $1, $2;
PREPARE e1_286(mood286) AS SELECT $1;
PREPARE e2_286(pair286) AS SELECT ($1).b;
PREPARE b1_286 AS SELECT 1 WHERE $1;
PREPARE j1_286 AS SELECT '{"a":1}'::jsonb -> $1;
PREPARE t1_286 AS SELECT current_date + $1;
SELECT name, parameter_types, result_types FROM pg_prepared_statements ORDER BY name;
EXECUTE d1_286(5);
EXECUTE d1_286(-1);
EXECUTE d2_286(5);
EXECUTE d2_286(-1);
EXECUTE e1_286('ok');
EXECUTE e1_286('bad');
EXECUTE e2_286(ROW(1, 'x'));
EXECUTE b1_286(true);
EXECUTE j1_286('a');
CREATE TABLE t286 (i int, n numeric, s text, d date, iv interval, j jsonb, a int[]);
INSERT INTO t286 VALUES (1, 1.5, 'x', '2020-01-01', '1 day', '{"a":1,"b":[1,2]}', '{1,2}');
SELECT i + '2', n * '2', s || 'y', d - '2020-01-01', iv * '2', '2' * iv, iv + '1 hour' FROM t286;
SELECT j -> 'a', j ->> 'b', j #> '{b,0}', j - 'a', a || '{3}', a @> '{1}' FROM t286;
SELECT date '2020-01-01' + '1';
SELECT '1' + '2';
SELECT 1 WHERE 'x';
