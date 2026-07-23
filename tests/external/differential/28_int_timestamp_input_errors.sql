-- Integer and timestamp input errors, told apart the way PostgreSQL tells them
-- apart. An integer text that overflows names the value and the type (22003); a
-- text not shaped like an integer is a syntax error naming the type (22P02);
-- and a value-to-value cast that overflows is value-less. A timestamp names
-- itself, not the date subfield it happens to parse first.

-- integer casts: overflow vs syntax vs value-to-value
SELECT '99999999999999999999'::int4;
SELECT '3000000000'::int4;
SELECT '-2147483649'::int4;
SELECT '40000'::int2;
SELECT '99999999999999999999'::int8;
SELECT '9223372036854775808'::int8;
SELECT 'abc'::int4;
SELECT '12abc'::int4;
SELECT ''::int4;
SELECT '0xGG'::int4;
SELECT (3000000000::int8)::int4;
SELECT (40000::int4)::int2;
SELECT '2147483647'::int4;
SELECT '-2147483648'::int4;
SELECT '42'::int4;
SELECT '0x1F'::int4;
SELECT '1_000'::int4;

-- range bounds: the element type's range is enforced, value named on overflow
SELECT '[99999999999999999999,5)'::int4range;
SELECT '[3000000000,5)'::int4range;
SELECT '[5,3000000000)'::int4range;
SELECT '[99999999999999999999,5)'::int8range;
SELECT '[a,5)'::int4range;
SELECT '[1,5)'::int4range::text;
SELECT '[10,20)'::int8range::text;

-- timestamp input names the timestamp type, not date; out-of-range kept
SELECT 'garbage'::timestamp;
SELECT 'garbage'::timestamptz;
SELECT 'x'::timestamp;
SELECT '2020-99-99'::timestamp;
SELECT '2020-01-01 badtime'::timestamptz;
SELECT '2020-01-01'::timestamp::text;
SELECT '2020-01-01 12:30:00'::timestamptz::text;
