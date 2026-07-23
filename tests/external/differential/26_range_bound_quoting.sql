-- A range bound is stored and shown as its element type prints it, quoted when
-- that text carries a character that would otherwise be structural. A timestamp
-- bound gains both its time-of-day and its surrounding quotes; an integer or
-- date bound needs neither. And a range literal written the way PostgreSQL
-- writes one — with quoted bounds — parses back to the same value, so a value
-- copied from PostgreSQL loads here.

-- output: timestamp bounds are normalized and quoted
SELECT '[2020-01-01,2020-02-01)'::tsrange::text;
SELECT '[2020-01-01,2020-02-01)'::tstzrange::text;
SELECT '[2020-01-01 12:30,2020-01-02)'::tsrange::text;
SELECT tsrange('2020-01-01', '2020-02-01')::text;
SELECT '{[2020-01-01,2020-02-01)}'::tsmultirange::text;
SELECT '{[2020-01-01,2020-02-01),[2020-06-01,2020-07-01)}'::tsmultirange::text;

-- input: PostgreSQL's quoted form round-trips
SELECT '["2020-01-01 00:00:00","2020-02-01 00:00:00")'::tsrange::text;
SELECT '["2020-01-01","2020-02-01")'::daterange::text;
SELECT '["2020-01-01 00:00:00+00","2020-02-01 00:00:00+00")'::tstzrange::text;

-- normalization: sloppy but valid bounds become canonical
SELECT '[2020-1-1,2020-2-1)'::daterange::text;
SELECT '[01,05)'::int4range::text;
SELECT '[  2020-01-01 , 2020-02-01 )'::tsrange::text;

-- bounds that need no quotes stay bare
SELECT '[1,5)'::int4range::text;
SELECT '[1.50,5.50)'::numrange::text;
SELECT '[2020-01-01,2020-02-01)'::daterange::text;
SELECT 'empty'::tsrange::text;
SELECT '(,)'::int4range::text;
SELECT '[1,10]'::int8range::text;
SELECT '(,5]'::int4range::text;
SELECT '[10,)'::int8range::text;

-- the quoting must not disturb equality, containment, or the bound accessors
SELECT '[2020-01-01,2020-02-01)'::tsrange = '["2020-01-01 00:00:00","2020-02-01 00:00:00")'::tsrange;
SELECT '[2020-01-01,2020-03-01)'::tsrange @> '2020-02-01'::timestamp;
SELECT '[2020-01-01,2020-03-01)'::tsrange @> '[2020-01-15,2020-02-01)'::tsrange;
SELECT lower('[2020-01-01,2020-02-01)'::tsrange);
SELECT upper('[2020-01-01,2020-02-01)'::tsrange);
SELECT '[2020-01-01,2020-02-01)'::tsrange -|- '[2020-02-01,2020-03-01)'::tsrange;
SELECT numrange(1.5, 3.5) @> 2.0;

-- and the range that a text cast produces parses back to itself
SELECT ('[2020-01-01,2020-02-01)'::tsrange::text)::tsrange::text;
