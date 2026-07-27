-- Date-time types render in ISO 8601 inside JSON (to_json / row_to_json /
-- json_build_object / json_agg / array_to_json), matching PostgreSQL 18: a `T`
-- between date and time, and a full `+HH:MM` offset for timestamptz — distinct
-- from the space-separated `::text` form.
--
-- Pin the session zone: a timestamptz renders relative to it.
SET TimeZone = 'UTC';

-- Plain timestamp: `T` separator, no offset; fractional seconds trim trailing 0s.
SELECT to_json('2020-01-01 12:30:45'::timestamp);
SELECT to_json('2020-01-01 12:30:45.123456'::timestamp);
SELECT to_json('2020-01-01 12:30:45.100000'::timestamp);

-- timestamptz: `T` separator and a full `+00:00` offset.
SELECT to_json('2020-01-01 12:30:45+00'::timestamptz);
SELECT to_jsonb('2020-06-15 08:00:00+00'::timestamptz);

-- date / time / timetz / interval keep their ordinary text form in JSON.
SELECT to_json('2020-06-15'::date);
SELECT to_json('12:30:45'::time);
SELECT to_json('12:30:45+05'::timetz);
SELECT to_json('1 day 02:00:00'::interval);

-- Composed: object, array, and a whole-row projection.
SELECT json_build_object('t', '2020-06-15 08:00:00+00'::timestamptz, 'd', '2020-06-15'::date);
SELECT array_to_json(ARRAY['2020-01-01 00:00:00+00'::timestamptz, '2021-02-02 03:04:05+00'::timestamptz]);
SELECT row_to_json(r) FROM (SELECT 1 AS id, '2020-01-01 00:00:00+00'::timestamptz AS ts) r;
SELECT json_agg(x ORDER BY x) FROM (VALUES
  ('2020-01-01 00:00:00+00'::timestamptz), ('2019-12-31 23:59:59+00'::timestamptz)) v(x);
