-- An untyped literal takes its type from the operand it faces, including when
-- that operand is an array; syntax that desugars to an internal call stays
-- syntax rather than becoming a callable function; and a bare row constructor
-- is not a field-access target.

CREATE TABLE optype (a int[], b text[]);
INSERT INTO optype VALUES ('{1,2}', '{x,y}');

-- an unknown literal against an array operand
SELECT '{1,2}'::int[] @> '{1}';
SELECT '{1}' <@ '{1,2}'::int[];
SELECT '{1,2}'::int[] && '{2}';
SELECT '{1,2}'::int[] && '{9}';
SELECT '{1,2}'::int[] @> '{3}';
SELECT '{a,b}'::text[] @> '{a}';
SELECT '{1,2}'::int[] = '{1,2}';
SELECT '{1,2}'::int[] = '{2,1}';
SELECT '{1,2}'::int[] <> '{1}';
SELECT '{1,2}'::int[] < '{1,3}';
SELECT '{1,2}'::int[] @> ARRAY[1];
SELECT ARRAY[1,2] @> '{1}';
SELECT '{1,2}'::int[] @> NULL;
SELECT '{1,2}'::int[] @> '{x}';
SELECT a @> '{1}' FROM optype;
SELECT b @> '{x}' FROM optype;
SELECT a = '{1,2}' FROM optype;

-- the comparisons and containments that already worked
SELECT ARRAY[1] = ARRAY[1];
SELECT ARRAY[1,2] @> ARRAY[1];
SELECT 1 = 1;
SELECT '1'::int = 1;

-- SIMILAR TO and OVERLAPS are syntax; PostgreSQL has no function of either name
SELECT similar_to('abc','a%c');
SELECT overlaps(1,2,3,4);
SELECT 'abc' SIMILAR TO 'a%c';
SELECT 'abc' NOT SIMILAR TO 'a%c';
SELECT 'abc' SIMILAR TO 'a%c' ESCAPE '#';
SELECT 'a%c' SIMILAR TO 'a#%c' ESCAPE '#';
SELECT (DATE '2020-01-01', DATE '2020-06-01') OVERLAPS (DATE '2020-03-01', DATE '2020-09-01');
SELECT (DATE '2020-01-01', DATE '2020-02-01') OVERLAPS (DATE '2020-03-01', DATE '2020-09-01');

-- a row constructor needs the extra parentheses before a field access
SELECT (1,2).f1;
SELECT ((1,2)).f1;
SELECT ((1,2)).f2;
SELECT (ROW(1,2)).f1;
SELECT (1,2);
SELECT ROW(1,2);
SELECT (1+2)*3;
SELECT (1,2) = (1,2);
SELECT (1,2) IN ((1,2));

DROP TABLE optype;
