-- Zones in timestamp and time input: numeric offsets, zone names resolved at
-- the value's own date, abbreviations (fixed, daylight, and those whose
-- meaning changed), the `dst` modifier, and PostgreSQL's errors for unknown
-- names and offsets out of range.
SET timezone = 'America/New_York';
SELECT '2024-01-05 12:00 EST'::timestamptz, '2024-01-05 12:00 Europe/Paris'::timestamptz, '2024-07-05 12:00 PST8PDT'::timestamptz;
SELECT '2024-01-05 12:00 FOO'::timestamptz;
SELECT '2024-01-05 12:00 Mars/Olympus'::timestamptz;
SELECT '2024-01-05 12:00 +16'::timestamptz;
SELECT '2024-01-05 12:00 +99999999999'::timestamptz;
SELECT '2024-01-05 12:00 +05:30'::timestamptz, '2024-01-05 12:00 -0800'::timestamptz, '1999-01-08 04:05:06 -8:00'::timestamptz, '1999-01-08 04:05:06 +05:30:15'::timestamptz;
SELECT '2024-02-05T10:00:00+0530'::timestamptz, '2024-02-05T10:00:00.5+05'::timestamptz, '2024-01-05T12:00:00.123456789Z'::timestamptz;
SELECT 'Mon, 05 Feb 2024 10:00:00 GMT'::timestamptz, '2024-01-05 12:00 z'::timestamptz, '2024-01-05 12:00 america/new_york'::timestamptz;
SELECT '2024-03-10 02:30 America/New_York'::timestamptz, '2024-11-03 01:30 America/New_York'::timestamptz;
SELECT '2024-03-10 02:30'::timestamptz, '2024-11-03 01:30'::timestamptz;
SELECT '2024-01-05 12:00 EDT'::timestamptz, '2024-07-05 12:00 EST'::timestamptz;
SELECT '2024-01-05 12:00 MSK'::timestamptz, '2010-07-05 12:00 MSK'::timestamptz;
SELECT '2024-01-05 12:00 EST DST'::timestamptz, '2024-01-05 12:00 DST EST'::timestamptz;
SELECT '2024-01-05 12:00 EDT DST'::timestamptz;
SELECT '2024-01-05 12:00 Europe/Paris DST'::timestamptz;
SELECT '2024-01-05 12:00 DST'::timestamptz;
SELECT '2024-01-05 12:00 Etc/GMT+5'::timestamptz, 'Jan 5 120000 EST 2024'::timestamptz;
SELECT '2024-01-05 12:00 America/New_York'::timestamp, '2024-01-05 12:00 +05'::date;
SELECT '12:00 EST'::timetz, '12:00+05:30'::timetz, '12:00 -0800'::timetz, '12:00 EST DST'::timetz;
SELECT '12:00 Etc/GMT+5'::timetz, '12:00 Asia/Kolkata'::timetz, '2024-01-05 12:00 America/New_York'::timetz;
SELECT '12:00 America/New_York'::timetz;
SELECT '12:00 America/New_York'::time;
SELECT '12:00 dst'::timetz;
SELECT 'allballs'::timetz, '12:00 z'::timetz, '12:00:00-05'::time;
SET timezone = 'UTC';
SELECT '2024-01-05 12:00'::timetz, '12:00'::timetz, '2024-07-05 12:00 Europe/Paris'::timetz;
