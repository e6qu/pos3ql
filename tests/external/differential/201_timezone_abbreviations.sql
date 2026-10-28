SET timezone = 'UTC';

SELECT '2021-01-01 12:00 EST'::timestamptz = '2021-01-01 17:00+00'::timestamptz
   AND '2021-06-01 12:00 eSt'::timestamptz = '2021-06-01 17:00+00'::timestamptz
   AND '2021-01-01 12:00 EDT'::timestamptz = '2021-01-01 16:00+00'::timestamptz
   AND '2021-06-01 12:00 HST'::timestamptz = '2021-06-01 22:00+00'::timestamptz
   AND '2021-06-01 12:00 MST'::timestamptz = '2021-06-01 19:00+00'::timestamptz
   AND '2021-06-01 12:00 CST'::timestamptz = '2021-06-01 18:00+00'::timestamptz
   AND '2021-06-01 12:00 Etc/GMT+5'::timestamptz = '2021-06-01 17:00+00'::timestamptz
   AND '2021-06-01 12:00 America/Port-au-Prince'::timestamptz = '2021-06-01 16:00+00'::timestamptz;

SELECT '12:00 EST'::timetz = '12:00-05'::timetz
   AND '2021-06-01 12:00 EST'::timestamp = '2021-06-01 12:00'::timestamp
   AND ('2021-06-01 12:00+00'::timestamptz AT TIME ZONE 'EST') = '2021-06-01 07:00'::timestamp
   AND to_timestamp('2021-06-01 12:00 EST', 'YYYY-MM-DD HH24:MI TZ') = '2021-06-01 17:00+00'::timestamptz;

SELECT '2012-06-01 12:00 MSK'::timestamptz = '2012-06-01 08:00+00'::timestamptz
   AND '2021-06-01 12:00 MSK'::timestamptz = '2021-06-01 09:00+00'::timestamptz
   AND to_timestamp('2012-06-01 12:00 MSK', 'YYYY-MM-DD HH24:MI TZ') = '2012-06-01 08:00+00'::timestamptz;

SELECT to_timestamp('2021-06-01 12:00 +03 EST', 'YYYY-MM-DD HH24:MI TZH TZ') = '2021-06-01 17:00+00'::timestamptz
   AND to_timestamp('2021-06-01 12:00 EST +03', 'YYYY-MM-DD HH24:MI TZ TZH') = '2021-06-01 09:00+00'::timestamptz
   AND to_timestamp('2021-06-01 12:00 +03:15 EST 45', 'YYYY-MM-DD HH24:MI TZH:TZM TZ TZM') = '2021-06-01 08:15+00'::timestamptz;

SELECT count(*) = 3 FROM pg_timezone_abbrevs
WHERE (abbrev = 'EST' AND extract(epoch FROM utc_offset) = -18000 AND NOT is_dst)
   OR (abbrev = 'EDT' AND extract(epoch FROM utc_offset) = -14400 AND is_dst)
   OR (abbrev = 'HST' AND extract(epoch FROM utc_offset) = -36000 AND NOT is_dst);

SET timezone = 'America/New_York';

SELECT '2021-06-01 12:00 LMT'::timestamptz = '2021-06-01 16:56:02+00'::timestamptz;

SELECT count(*) = 1 FROM pg_timezone_abbrevs
WHERE abbrev = 'LMT' AND extract(epoch FROM utc_offset) = -17762 AND NOT is_dst;

SET timezone = 'Asia/Shanghai';

SELECT '2021-06-01 12:00 CST'::timestamptz = '2021-06-01 04:00+00'::timestamptz
   AND ('2021-06-01 12:00+00'::timestamptz AT TIME ZONE 'CST') = '2021-06-01 20:00'::timestamp;

SELECT count(*) = 1 FROM pg_timezone_abbrevs
WHERE abbrev = 'CST' AND extract(epoch FROM utc_offset) = 28800 AND NOT is_dst;

SET timezone = 'Asia/Kolkata';

SELECT '2021-06-01 12:00 IST'::timestamptz = '2021-06-01 06:30+00'::timestamptz;

SELECT count(*) = 1 FROM pg_timezone_abbrevs
WHERE abbrev = 'IST' AND extract(epoch FROM utc_offset) = 19800 AND NOT is_dst;

SET timezone = 'UTC';
