-- POSIX time zone specifications: offsets count hours west of UTC, names
-- are upper-cased, and daylight rules use the M, J and plain day forms.
SET TimeZone = 'UTC+3';
SHOW TimeZone;
SELECT '2024-01-05 10:00'::timestamptz, to_char('2024-01-05 10:00'::timestamptz, 'TZ OF');
SET TimeZone = 'xyz5';
SHOW TimeZone;
SELECT '2024-07-05 10:00'::timestamptz, to_char('2024-07-05 10:00'::timestamptz, 'TZ');
SELECT abbrev, utc_offset, is_dst FROM pg_timezone_abbrevs WHERE abbrev IN ('XYZ', 'ABC') ORDER BY 1;
-- A daylight name alone takes PostgreSQL's default United States rules.
SET TimeZone = 'xyz5abc';
SHOW TimeZone;
SELECT '2024-07-05 10:00'::timestamptz, '2024-01-05 10:00'::timestamptz,
  '2024-03-10 02:30'::timestamptz, '2024-11-03 01:30'::timestamptz;
SELECT to_char('2024-07-05 10:00'::timestamptz, 'TZ'), '2024-07-05 10:00 abc'::timestamptz;
SELECT abbrev, utc_offset, is_dst FROM pg_timezone_abbrevs WHERE abbrev IN ('XYZ', 'ABC') ORDER BY 1;
SET TimeZone = 'xyz5abc4';
SELECT '2024-07-05 10:00'::timestamptz, '2024-01-05 10:00'::timestamptz;
SET TimeZone = 'foo3bar,M4.1.0/1,J300/3';
SHOW TimeZone;
SELECT '2024-07-05 10:00'::timestamptz, '2024-11-05 10:00'::timestamptz;
SET TimeZone = 'xyz5abc,J60,300';
SELECT '2024-02-29 12:00'::timestamptz, '2024-03-01 12:00'::timestamptz,
  '2023-03-01 12:00'::timestamptz, '2024-10-27 12:00'::timestamptz,
  '2024-10-28 12:00'::timestamptz;
SET TimeZone = 'xyz-10abc,M10.1.0,M4.1.0/3';
SELECT '2024-07-05 10:00'::timestamptz, '2024-01-05 10:00'::timestamptz,
  '2024-04-07 02:30'::timestamptz, '2024-10-06 02:30'::timestamptz;
SET TimeZone = 'xyz5abc,0/0,J365/25';
SELECT '2024-07-05 10:00'::timestamptz, '2024-01-05 10:00'::timestamptz;
SET TimeZone = 'xyz5abc,M3.2.0/-1,M11.1.0/167';
SELECT '2024-03-09 23:30'::timestamptz;
SET TimeZone = 'xyz100';
SELECT '2024-07-05 10:00'::timestamptz;
SET TimeZone = 'xyz5:59:60';
SELECT '2024-07-05 10:00'::timestamptz;
SET TimeZone = '<+0530>-5:30';
SHOW TimeZone;
SELECT '2024-07-05 10:00'::timestamptz;
SET TimeZone = '<UTC+3>-3';
SELECT to_char('2024-07-05 10:00'::timestamptz, 'TZ');
SET TimeZone = '<>-3';
SELECT '2024-07-05 10:00'::timestamptz, to_char('2024-07-05 10:00'::timestamptz, 'TZ');
SET TimeZone = 'abcdefghijklmnop5';
SHOW TimeZone;
SET datestyle = postgres;
SELECT '2024-07-05 10:00'::timestamptz;
SET datestyle = sql;
SET TimeZone = 'xyz5abc,M3.2.0,M11.1.0';
SELECT '2024-07-05 10:00'::timestamptz;
RESET datestyle;
-- Names that are not installed zones and not valid specifications.
SET TimeZone = 'abc';
SET TimeZone = 'x';
SET TimeZone = 'xyz168';
SET TimeZone = 'xyz5:60';
SET TimeZone = 'xyz5abc,M3.2.0';
SET TimeZone = 'xyz5abc;M3.2.0,M11.1.0';
SET TimeZone = 'abc5def,M13.1.0,M1.1.0';
SET TimeZone = 'xyz5abc,J0,300';
SET TimeZone = 'xyz5abc,366,300';
SET TimeZone = 'xyz5abc,M3.2.0/168,M11.1.0';
SET TimeZone = 'xyz5abc4x';
SET TimeZone = '<abc-3';
SET TimeZone = ':xyz5';
SET TimeZone = ' utc';
SET TimeZone = 'z';
-- Seconds in the offset look like leap seconds.
SET TimeZone = 'xyz5:00:30';
SET TimeZone = '<+03:00:30>-03:00:30';
-- Installed zones keep their spelling; POSIX names are upper-cased.
SET TimeZone = 'utc';
SHOW TimeZone;
SET TimeZone = 'america/new_york';
SHOW TimeZone;
SET TimeZone = ':UTC';
SHOW TimeZone;
SET TimeZone = 'gmt';
SHOW TimeZone;
SET TimeZone = 'gmt+3';
SHOW TimeZone;
SELECT '2024-07-05 10:00'::timestamptz, to_char('2024-07-05 10:00'::timestamptz, 'TZ');
SET TimeZone = 'Etc/GMT+15';
SHOW TimeZone;
SET TimeZone = 'Europe/Berlin5';
SHOW TimeZone;
-- Numeric settings name the fixed zone pg_tzset_offset builds.
SET TimeZone = '-3';
SHOW TimeZone;
SELECT to_char('2024-07-05 10:00'::timestamptz, 'TZ'), '2024-01-01 00:00'::timestamptz;
SET TimeZone = 3.0001;
SHOW TimeZone;
SET TimeZone = 200;
SET TIME ZONE INTERVAL '-3:30:15';
SHOW TimeZone;
SET TIME ZONE INTERVAL '200 hours';
SET TIME ZONE INTERVAL '1 day';
SET TIME ZONE INTERVAL '1 month';
SET TimeZone = 'UTC';
-- Date/time input and zone conversion accept the same specifications.
SELECT '2024-01-05 10:00 UTC+3'::timestamptz, '2024-01-05 10:00 utc-3'::timestamptz;
SELECT '2024-01-05 10:00 xyz5'::timestamptz, '2024-01-05 10:00 EST5EDT'::timestamptz;
SELECT '2024-07-05 10:00 foo3bar'::timestamptz, '2024-01-05 10:00 GMT+5'::timestamptz;
SELECT '2024-01-05 10:00 xyz5abc'::timestamptz, '2024-07-05 10:00 xyz5abc'::timestamptz;
SELECT '2024-01-05 10:00 UTC+3:30'::timestamptz, '10:00 xyz5'::timetz;
SELECT '2024-01-05 10:00 abc'::timestamptz;
SELECT '2024-01-05 10:00 xyz168'::timestamptz;
SELECT '2024-07-05 10:00 xyz5abc,M3.2.0,M11.1.0'::timestamptz;
SELECT timezone('UTC+3', '2024-01-05 10:00'::timestamptz);
SELECT timezone('xyz5abc', '2024-07-05 10:00'::timestamp);
SELECT timezone('3', '2024-01-05 10:00'::timestamptz), timezone('-3', '2024-01-05 10:00'::timestamptz);
SELECT timezone('xyz5:00:30', '2024-01-05 10:00'::timestamptz);
SELECT '2024-01-05 10:00'::timestamp AT TIME ZONE 'UTC+3';
SELECT '10:00+00'::timetz AT TIME ZONE 'UTC+3';
SELECT '2024-01-05 10:00'::timestamp AT TIME ZONE 'bad5,';
SELECT timezone('', '2024-01-05 10:00'::timestamptz);
SELECT timezone(repeat('a', 254) || '5', '2024-01-05 10:00'::timestamptz);
SELECT timezone(repeat('a', 255) || '5', '2024-01-05 10:00'::timestamptz);
SELECT count(*) FROM pg_timezone_names WHERE name IN ('UTC+3', 'XYZ5');
