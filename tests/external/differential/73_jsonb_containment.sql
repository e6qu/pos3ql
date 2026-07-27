-- jsonb deep containment: `@>` and `<@`, matching PostgreSQL 18.
-- (json has no containment operator — that is verified in the error corpora.)
DROP TABLE IF EXISTS jc;

-- Object containment: key subset with deep value matching.
SELECT '{"a":1,"b":2}'::jsonb @> '{"a":1}'::jsonb;
SELECT '{"a":1}'::jsonb @> '{"a":1,"b":2}'::jsonb;
SELECT '{"a":1}'::jsonb @> '{}'::jsonb;
SELECT '{"a":{"b":2,"c":3}}'::jsonb @> '{"a":{"b":2}}'::jsonb;
SELECT '{"a":[1,2]}'::jsonb @> '{"a":[2]}'::jsonb;

-- Array containment, and the primitive-in-array exception.
SELECT '[1,2,3]'::jsonb @> '[1,2]'::jsonb;
SELECT '[1,2,3]'::jsonb @> '[2,1]'::jsonb;
SELECT '[1,1]'::jsonb @> '[1,1,1]'::jsonb;
SELECT '[1,2,3]'::jsonb @> '2'::jsonb;
SELECT '[1,2,3]'::jsonb @> '"a"'::jsonb;
SELECT '[[1,2]]'::jsonb @> '[[1]]'::jsonb;

-- Scalars, numeric-value equality (1.0 = 1), and type-mismatch non-containment.
SELECT '1'::jsonb @> '1'::jsonb;
SELECT '1.0'::jsonb @> '1'::jsonb;
SELECT 'null'::jsonb @> 'null'::jsonb;
SELECT '"x"'::jsonb @> '"x"'::jsonb;
SELECT '{"a":1}'::jsonb @> '[1]'::jsonb;
SELECT '[{"a":1}]'::jsonb @> '{"a":1}'::jsonb;

-- <@ is @> with the operands swapped.
SELECT '{"a":1}'::jsonb <@ '{"a":1,"b":2}'::jsonb;
SELECT '[1,2,3]'::jsonb <@ '[1,2]'::jsonb;

-- NULL propagation.
SELECT NULL::jsonb @> '{"a":1}'::jsonb;

-- Against a jsonb column, in a WHERE filter.
CREATE TABLE jc (id int, doc jsonb);
INSERT INTO jc VALUES (1, '{"tags":["x","y"],"n":5}'), (2, '{"tags":["z"],"n":9}');
SELECT id FROM jc WHERE doc @> '{"tags":["x"]}' ORDER BY id;
SELECT id FROM jc WHERE doc @> '{"n":9}' ORDER BY id;

DROP TABLE jc;
