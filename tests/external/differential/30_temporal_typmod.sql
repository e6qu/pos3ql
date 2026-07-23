-- A temporal type's atttypmod is the bare precision (0..6), with no 4-byte
-- header — unlike varchar(n) or numeric(p,s) — and interval packs the
-- field-range mask into the high half. So the same integer means the same
-- thing to both engines, and format_type renders it the same. The rounding a
-- precision drives is unchanged.

CREATE TABLE ttm (
  a timestamp(3),
  b timestamptz(4),
  c time(2),
  d timetz(5),
  e interval(1),
  f timestamp(0),
  g timestamp,
  h varchar(5),
  i numeric(6,2),
  j char(3)
);

-- the modifier each column stores, and how it renders
SELECT attname, atttypmod FROM pg_attribute
 WHERE attrelid = 'ttm'::regclass AND attnum > 0 ORDER BY attnum;
SELECT format_type(atttypid, atttypmod) FROM pg_attribute
 WHERE attrelid = 'ttm'::regclass AND attnum > 0 ORDER BY attnum;

-- format_type over the temporal oids directly
SELECT format_type(1114, 3), format_type(1184, 4), format_type(1083, 2), format_type(1266, 5);
SELECT format_type(1186, 2147418113), format_type(1186, -1), format_type(1114, -1);
SELECT format_type(1114, 0);

-- the precision still rounds the value, at every field and precision
SELECT '12:34:56.789123'::time(2);
SELECT '12:34:56.789123'::time(0);
SELECT '2020-01-01 00:00:00.123456'::timestamp(3);
SELECT '2020-01-01 12:00:00.5'::timestamp(0);
SELECT '2020-01-01 00:00:00.123456'::timestamptz(4);
SELECT '1.234567 sec'::interval(1);
SELECT '1.9 sec'::interval(0);
SELECT '00:00:01.123456'::interval(6);

-- no modifier passes through, and the other typmod-bearing types are unaffected
SELECT '12:34:56.789123'::time;
SELECT '2020-01-01'::timestamp;
SELECT 'abcdef'::varchar(3);
SELECT 1.005::numeric(6,2);
SELECT B'101'::bit(3);

DROP TABLE ttm;
