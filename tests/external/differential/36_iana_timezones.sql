-- The full IANA time-zone database via TZif: historical rules, rule changes,
-- fractional-hour zones, case-insensitive names, and zone names in timestamp
-- literals. Timestamps are historical/settled so tzdata-version skew between
-- the reference and the system database cannot bite.
SET timezone = 'UTC';

-- Rule changes no single POSIX rule can express: Moscow's +04 era (2011-2014).
SET timezone = 'Europe/Moscow';
SELECT '2012-07-01 12:00:00+00'::timestamptz, '2021-07-01 12:00:00+00'::timestamptz;
SELECT '2010-07-01 12:00:00+00'::timestamptz;

-- Venezuela's -04:30 era (2007-2016).
SET timezone = 'America/Caracas';
SELECT '2010-01-01 00:00:00+00'::timestamptz, '2021-01-01 00:00:00+00'::timestamptz;

-- Historical US DST: 1968 began the last Sunday of April.
SET timezone = 'America/New_York';
SELECT '1968-04-15 12:00:00+00'::timestamptz, '1968-05-15 12:00:00+00'::timestamptz;
SELECT '2021-03-14 06:59:00+00'::timestamptz, '2021-03-14 07:01:00+00'::timestamptz;

-- Fractional-hour zones, including half-hour DST.
SET timezone = 'Australia/Lord_Howe';
SELECT '2021-07-01 12:00:00+00'::timestamptz, '2021-01-15 12:00:00+00'::timestamptz;
SET timezone = 'Pacific/Chatham';
SELECT '2021-07-01 00:00:00+00'::timestamptz, '2021-01-15 00:00:00+00'::timestamptz;
SET timezone = 'Asia/Kathmandu';
SELECT '2021-07-01 00:00:00+00'::timestamptz;
SET timezone = 'Asia/Kolkata';
SELECT '2021-07-01 00:00:00+00'::timestamptz;

-- Case-insensitive names.
SET timezone = 'aMeRiCa/CHICAGO';
SELECT '2021-01-15 12:00:00+00'::timestamptz;

-- Southern hemisphere with historical rules.
SET timezone = 'America/Santiago';
SELECT '2021-01-15 12:00:00+00'::timestamptz, '2021-07-15 12:00:00+00'::timestamptz;
SET timezone = 'Africa/Casablanca';
SELECT '2021-01-15 12:00:00+00'::timestamptz;

-- Pre-1970 Europe.
SET timezone = 'Europe/Paris';
SELECT '1975-06-01 00:00:00+00'::timestamptz, '1960-01-01 00:00:00+00'::timestamptz;

-- Far future: the footer rule carries past the recorded transitions.
SET timezone = 'America/Los_Angeles';
SELECT '2088-01-15 12:00:00+00'::timestamptz, '2088-07-15 12:00:00+00'::timestamptz;

-- AT TIME ZONE with database zones.
SET timezone = 'UTC';
SELECT '2021-07-04 12:00:00+00'::timestamptz AT TIME ZONE 'Asia/Tehran';
SELECT '2012-07-04 12:00:00+00'::timestamptz AT TIME ZONE 'Europe/Moscow';
SELECT '2021-07-04 12:00:00' AT TIME ZONE 'Australia/Lord_Howe';

-- Zone names inside timestamp literals, resolved at the literal's instant.
SELECT '2021-01-01 00:00 Europe/Moscow'::timestamptz;
SELECT '2012-07-01 00:00 Europe/Moscow'::timestamptz;
SELECT '2021-06-01 12:00:00 EST'::timestamptz;
SELECT '2021-06-01 12:00:00 America/New_York'::timestamptz;
SELECT '2021-06-01 12:00 America/New_York'::timestamp;
SELECT '2021-06-01 12:00 Nosuch/Zone'::timestamptz;

-- A bare timestamptz literal reads in the session zone, at the literal's
-- own instant (Moscow's +04 era applies to a 2012 literal).
SET timezone = 'Europe/Moscow';
SELECT '2021-01-01 12:00'::timestamptz, '2012-07-01 12:00'::timestamptz;
SET timezone = 'UTC';

-- Unknown zones refuse loudly in SET too.
SET timezone = 'Not/A_Zone';
SHOW timezone;
SET timezone = 'UTC';
