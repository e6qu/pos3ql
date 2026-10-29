-- FETCH renders retained typed values using the current session settings.
-- Reference: PostgreSQL 18 sql-declare and runtime-config-client documentation.
-- Empty target lists: src/backend/parser/gram.y, REL_18_STABLE, simple_select.
SET DateStyle = 'ISO, MDY';
SET TimeZone = 'UTC';
SET bytea_output = 'hex';
SET extra_float_digits = 3;
SELECT;
SELECT ALL FROM generate_series(1, 2);
SELECT FROM generate_series(1, 3) WHERE false;
SELECT FROM generate_series(1, 3) LIMIT 1 OFFSET 1;
SELECT FROM (SELECT) AS empty_source;
SELECT UNION ALL SELECT;
BEGIN;
DECLARE empty_values CURSOR FOR SELECT;
FETCH ALL FROM empty_values;
COMMIT;
BEGIN;
DECLARE rendering SCROLL CURSOR WITH HOLD FOR
  SELECT DATE '2020-01-02', ARRAY[DATE '2020-01-02'],
         ROW(DATE '2020-01-02', TIMESTAMPTZ '2020-01-02 03:04:05+00'),
         '\x5c00'::bytea, 1.234567891234567::float8
    FROM generate_series(1, 3);
FETCH NEXT FROM rendering;
COMMIT;
SET DateStyle = 'SQL, DMY';
SET TimeZone = 'Europe/Bucharest';
SET bytea_output = 'escape';
SET extra_float_digits = 0;
FETCH NEXT FROM rendering;
SET DateStyle = 'German';
SET TimeZone = 'UTC';
FETCH NEXT FROM rendering;
CLOSE rendering;
SET DateStyle = 'ISO, MDY';
SET bytea_output = 'hex';
SET extra_float_digits = 3;
