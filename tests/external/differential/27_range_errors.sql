-- A malformed range literal and a bad bound value are two different errors, as
-- PostgreSQL has them. A literal that is not shaped like a range names neither
-- the range type nor the element — only that it is malformed. A bound that is
-- well-placed but is not a value of the element type raises that element type's
-- own input error, naming the type and the offending value.

-- structural: wrong shape, no type name
SELECT 'garbage'::int4range;
SELECT '1,5)'::int4range;
SELECT '[1,5'::int4range;
SELECT '[1,2,3]'::int4range;
SELECT '[1,2,3,4)'::int8range;
SELECT 'x'::numrange;

-- element: the value is not of the element type, named as that type's error
SELECT '[a,5)'::int4range;
SELECT '[1,z)'::int4range;
SELECT '[5,b)'::int8range;
SELECT '[1.5,x)'::numrange;
SELECT '[x,5)'::numrange;
SELECT '[a,b)'::int4range;
SELECT '[x,y)'::daterange;

-- the offending side is the one named, whichever it is
SELECT '[1,bad)'::int4range;
SELECT '[bad,9)'::int8range;

-- valid literals are unaffected
SELECT '[1,5)'::int4range::text;
SELECT '[1.5,5.5)'::numrange::text;
SELECT '[2020-01-01,2020-02-01)'::daterange::text;
SELECT 'empty'::int4range::text;
SELECT '(,)'::int8range::text;
