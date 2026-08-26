-- The clock `TO`-range interval qualifiers, and mixed-sign interval rendering.
-- A range's value is a day count (only when it starts at DAY) followed by an
-- H:M:S clock, truncated to the trailing field; a two-part clock is H:M or M:S
-- per the leading field, a three-part clock is always H:M:S, and a bare number
-- takes the trailing field. Day and clock carry independent signs. On output, a
-- positive field takes an explicit `+` only when the field before it was
-- negative.

-- DAY-leading ranges
SELECT INTERVAL '1 2:03:04' DAY TO SECOND;
SELECT INTERVAL '1 2:03' DAY TO MINUTE;
SELECT INTERVAL '1 2' DAY TO HOUR;
SELECT INTERVAL '1 2:03:04' DAY TO HOUR;
SELECT INTERVAL '1 2:03:04.5' DAY TO MINUTE;
SELECT INTERVAL '1 25:00' DAY TO HOUR;
SELECT INTERVAL '-1 2:03' DAY TO MINUTE;
SELECT INTERVAL '1 -2:03' DAY TO MINUTE;

-- time-leading ranges
SELECT INTERVAL '2:03:04' HOUR TO SECOND;
SELECT INTERVAL '2:03' HOUR TO MINUTE;
SELECT INTERVAL '2:03:04' HOUR TO MINUTE;
SELECT INTERVAL '3:04' MINUTE TO SECOND;
SELECT INTERVAL '2:03:04.5' MINUTE TO SECOND;

-- a bare number takes the trailing field
SELECT INTERVAL '5' DAY TO HOUR;
SELECT INTERVAL '100' MINUTE TO SECOND;

-- malformed values and invalid field orderings error
SELECT INTERVAL 'bad' HOUR TO SECOND;
SELECT INTERVAL '1 x:03' DAY TO MINUTE;

-- mixed-sign rendering: a `+` appears only after a negative field
SELECT INTERVAL '-1 day 2 hours';
SELECT INTERVAL '1 day -2 hours';
SELECT INTERVAL '-1 month 5 days';
SELECT INTERVAL '-2 days -3 hours';
SELECT INTERVAL '1 year 2 mons -3 days';
SELECT INTERVAL '-1 year 2 mons 3 days';
SELECT INTERVAL '1 mon -3 days 4 hours';
SELECT INTERVAL '1 year 2 mons 3 days 04:05:06';
SELECT INTERVAL '1 day 2 hours';
SELECT INTERVAL '-1 day';
SELECT INTERVAL '2 hours';

-- A qualifier constrains an already unit-bearing value as well as a compact
-- scalar. Fractional coarse units cascade before an unqualified cast and are
-- truncated only by the declared trailing field.
SELECT INTERVAL '1 year 2 mons 3 days 04:05:06.789' YEAR;
SELECT INTERVAL '1 year 2 mons 3 days 04:05:06.789' MONTH;
SELECT INTERVAL '1 year 2 mons 3 days 04:05:06.789' DAY;
SELECT INTERVAL '1 year 2 mons 3 days 04:05:06.789' HOUR;
SELECT INTERVAL '1 year 2 mons 3 days 04:05:06.789' MINUTE;
SELECT INTERVAL '1 year 2 mons 3 days 04:05:06.789' SECOND(2);
SELECT CAST('1 day 02:03:04.567' AS interval hour to minute);
SELECT '1.5 years'::interval, '1.5 months'::interval,
       '1.5 weeks'::interval, '1.5 days'::interval,
       '1.1234567 seconds'::interval;

SET intervalstyle = postgres_verbose;
SELECT v::text, ARRAY[v]::text, ROW(v)::text, to_json(v)::text
  FROM (VALUES (interval '1 year 2 mons 3 days 04:05:06.789')) q(v);
SELECT interval '-1 mon 5 days -06:07:08.9';
SET intervalstyle = sql_standard;
SELECT interval '1 year 2 mons 3 days 04:05:06.789';
SELECT interval '-1 mon 5 days -06:07:08.9';
SET intervalstyle = iso_8601;
SELECT interval '1 year 2 mons 3 days 04:05:06.789';
SELECT interval '-1 mon 5 days -06:07:08.9';
RESET intervalstyle;

CREATE TABLE interval_typmod_probe (
  y interval year,
  m interval month,
  ym interval year to month,
  d interval day,
  h interval hour,
  mi interval minute,
  s interval second,
  ds interval day to second(3),
  fp interval(4)
);
INSERT INTO interval_typmod_probe VALUES (
  '1 year 2 mons 3 days 04:05:06.789',
  '1 year 2 mons 3 days 04:05:06.789',
  '1 year 2 mons 3 days 04:05:06.789',
  '1 year 2 mons 3 days 04:05:06.789',
  '1 year 2 mons 3 days 04:05:06.789',
  '1 year 2 mons 3 days 04:05:06.789',
  '1 year 2 mons 3 days 04:05:06.789',
  '1 day 02:03:04.56789',
  '1 day 02:03:04.56789'
);
SELECT * FROM interval_typmod_probe;
SELECT attname, atttypmod, format_type(atttypid, atttypmod)
  FROM pg_attribute
 WHERE attrelid = 'interval_typmod_probe'::regclass AND attnum > 0
 ORDER BY attnum;
SELECT column_name, datetime_precision, interval_type,
       interval_precision IS NULL
  FROM information_schema.columns
 WHERE table_name = 'interval_typmod_probe'
 ORDER BY ordinal_position;
CREATE DOMAIN interval_typmod_domain AS interval day to second(2);
SELECT domain_name, datetime_precision, interval_type,
       interval_precision IS NULL
  FROM information_schema.domains
 WHERE domain_name = 'interval_typmod_domain';
