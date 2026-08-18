-- Every modeled range and multirange has its PostgreSQL array identity.
CREATE TABLE diff_range_arrays (
  int4_values int4range[], int8_values int8range[], numeric_values numrange[],
  date_values daterange[], timestamp_values tsrange[], timestamptz_values tstzrange[],
  int4_multi int4multirange[], int8_multi int8multirange[], numeric_multi nummultirange[],
  date_multi datemultirange[], timestamp_multi tsmultirange[], timestamptz_multi tstzmultirange[]
);
INSERT INTO diff_range_arrays VALUES (
  ARRAY['[1,3)'::int4range], ARRAY['[1,3)'::int8range], ARRAY['[1.5,3.5)'::numrange],
  ARRAY['[2020-01-01,2020-01-03)'::daterange],
  ARRAY['[2020-01-01 00:00:00,2020-01-02 00:00:00)'::tsrange],
  ARRAY['[2020-01-01 00:00:00+00,2020-01-02 00:00:00+00)'::tstzrange],
  ARRAY['{[1,3)}'::int4multirange], ARRAY['{[1,3)}'::int8multirange],
  ARRAY['{[1.5,3.5)}'::nummultirange], ARRAY['{[2020-01-01,2020-01-03)}'::datemultirange],
  ARRAY['{[2020-01-01 00:00:00,2020-01-02 00:00:00)}'::tsmultirange],
  ARRAY['{[2020-01-01 00:00:00+00,2020-01-02 00:00:00+00)}'::tstzmultirange]
);
SELECT pg_typeof(int4_values), pg_typeof(int8_values), pg_typeof(numeric_values),
       pg_typeof(date_values), pg_typeof(timestamp_values), pg_typeof(timestamptz_values),
       pg_typeof(int4_multi), pg_typeof(int8_multi), pg_typeof(numeric_multi),
       pg_typeof(date_multi), pg_typeof(timestamp_multi), pg_typeof(timestamptz_multi)
  FROM diff_range_arrays;
SELECT int4_values::text, int8_values::text, numeric_values::text, date_values::text,
       timestamp_values::text, timestamptz_values::text, int4_multi::text, int8_multi::text,
       numeric_multi::text, date_multi::text, timestamp_multi::text, timestamptz_multi::text
  FROM diff_range_arrays;
DROP TABLE diff_range_arrays;
