-- Error-message fidelity: the main differential normalizes an error to its
-- SQLSTATE, so a message that names the wrong operator, the wrong type, or no
-- value at all is invisible to it. These statements compare the full ERROR
-- line, and each cluster below pins a wording fix that shipped guarded only by
-- unit tests before this corpus existed.

-- an undefined operator is reported under the operator that was written
SELECT true < 1;
SELECT true > 1;
SELECT true <> 1;
SELECT 1 + true;
SELECT ARRAY[1] + 1;
SELECT ARRAY['a'] + 1;
SELECT '{}'::json = '{}'::json;

-- an undefined function names the argument types it was called with
SELECT nosuchfunc(1);
SELECT nosuchfunc('a');
SELECT nosuchfunc();
SELECT nosuchfunc(1, 'a');
SELECT nosuchfunc(1::int8, true);
SELECT nosuchfunc(ARRAY[1]);
SELECT similar_to('a', 'a');

-- a malformed range literal names neither type nor element; a bad bound is the
-- element type's own error, naming the offending side
SELECT 'garbage'::int4range;
SELECT '[1,2,3]'::int4range;
SELECT '[a,5)'::int4range;
SELECT '[1,z)'::int8range;
SELECT '[1.5,x)'::numrange;

-- integer overflow names the value and type; malformed text is a syntax error;
-- a value-to-value cast overflows value-lessly
SELECT '3000000000'::int4;
SELECT '99999999999999999999'::int8;
SELECT '40000'::int2;
SELECT 'abc'::int4;
SELECT (3000000000::int8)::int4;
SELECT (40000::int4)::int2;

-- timestamp input names the timestamp type, not the date subfield
SELECT 'garbage'::timestamp;
SELECT 'garbage'::timestamptz;
SELECT '2020-99-99'::timestamp;

-- a boolean context refuses a non-boolean by name
SELECT true AND 1;
SELECT 1 OR false;
SELECT NOT 1;
SELECT true AND 1.5;

-- the escape clause takes one character
SELECT 'abc' LIKE 'a%c' ESCAPE 'xy';

-- GROUP BY / ORDER BY positions, and the ungrouped column, are named
SELECT 1 GROUP BY 5;
SELECT 1 ORDER BY 5;

-- interval qualifiers: out-of-range vs malformed are distinct errors
SELECT INTERVAL '1-13' YEAR TO MONTH;
SELECT INTERVAL 'x-2' YEAR TO MONTH;

-- DROP names the kind of object it could not find
DROP TABLE nosuchtable;
DROP VIEW nosuchview;

-- found by this corpus's first run: FROM-less GROUP BY positions were never
-- resolved, and an aggregate could be a grouping key
SELECT count(*) GROUP BY 1;
SELECT 1 GROUP BY 0;
SELECT 1 GROUP BY 2;
