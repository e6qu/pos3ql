-- Date input reads numbers in DateStyle's field order (MDY, DMY or YMD), a
-- year of three or more digits first in any order, and month names in any
-- position; a month or day out of range suggests another DateStyle.
SET timezone = 'UTC';
SELECT '05/01/2024'::date, '1/5/24'::date, '1/5/70'::date, '1/5/69'::date, '1/5/0'::date;
SELECT '13/01/2024'::date;
SELECT '01/32/2024'::date;
SELECT '02/30/2024'::date;
SELECT '2024-13-05'::date;
SELECT '2024-02-30'::date;
SELECT '0000-01-01'::date;
SELECT 'Jan 5 2024'::date, '5-Jan-2024'::date, '2024-Jan-05'::date, 'January 5, 2024'::date;
SELECT '2024 Jan 5'::date, 'Jan 2024 5'::date, 'Jan-5-2024'::date, '5 Jan 24'::date;
SELECT 'Fri Jan 05 2024 12:00:00'::timestamp, 'Thu Jan 05 2024'::date, 'on Friday at 12:00, Jan 5 2024'::timestamp;
SELECT 'jan5'::date;
SELECT '5jan2024'::date;
SELECT '1/8/1999'::date, '1/18/1999'::date;
SELECT '18/1/1999'::date;
SET datestyle = 'ISO, DMY';
SELECT '05/01/2024'::date, '1/8/1999'::date, '18/1/1999'::date, '01/02/03'::date;
SELECT '2024-Feb-03'::date, '03-Feb-2024'::date, 'Feb-03-24'::date;
SELECT '1/18/1999'::date;
SELECT '05/01/2024 13:30'::timestamp, '05/01/2024 13:30 +02'::timestamptz;
SET datestyle = 'ISO, YMD';
SELECT '24/05/01'::date, '01/02/03'::date, '99-Feb-03'::date, '03-Feb-2024'::date, '1999.008'::date;
SELECT '05/01/2024'::date;
SELECT '99008'::date;
SET datestyle = 'SQL, DMY';
SELECT '05/01/2024'::date, '05/01/2024 12:00'::timestamp;
SET datestyle = 'German';
SELECT '05.01.2024'::date;
SET datestyle = 'Postgres, MDY';
SELECT 'Fri Jan 05 12:00:00 2024 EST'::timestamptz, '2024-01-05 BC'::date;
RESET datestyle;
-- The setting applies to later statements of the same query string, to
-- arrays, ranges, stored rows and COPY.
SET datestyle = 'ISO, DMY'; SELECT '05/01/2024'::date; SET datestyle = 'ISO, MDY'; SELECT '05/01/2024'::date;
SELECT '{05/01/2024,2024-02-03}'::date[], '[05/01/2024,06/01/2024)'::daterange;
CREATE TABLE dio_rows (d date, t timestamp, z timestamptz, tt time, ttz timetz);
SET datestyle = 'SQL, DMY';
INSERT INTO dio_rows VALUES ('05/01/2024', '05/01/2024 13:00', '05/01/2024 13:00 +02', '1:00 pm', '1:00 pm +02');
COPY dio_rows FROM stdin;
06/01/2024	06/01/2024 14:00	06/01/2024 14:00 EST	2:00 PM	2:00 PM EST
\.
SELECT * FROM dio_rows ORDER BY d;
SET datestyle = 'ISO, MDY';
SELECT * FROM dio_rows ORDER BY d;
PREPARE dio_date(date) AS SELECT $1;
SET datestyle = 'ISO, DMY';
EXECUTE dio_date('05/01/2024');
DEALLOCATE dio_date;
DROP TABLE dio_rows;
