-- Zone offsets are PostgreSQL 18.6's own zone data (tzdata 2026c), whatever
-- release the host's zone files have: Bogota's 1992 daylight time starts on
-- May 3, and Winnipeg and Havana still change clocks in 2027.
SET TimeZone = 'UTC';
SELECT '1992-05-02 12:00:00 America/Bogota'::timestamptz, '1965-03-31 12:00 America/Sao_Paulo'::timestamptz;
SELECT '2027-01-15 12:00 America/Winnipeg'::timestamptz, '2027-07-15 12:00 America/Winnipeg'::timestamptz;
SELECT timezone('America/Winnipeg', '2027-01-15 12:00Z'::timestamptz);
SET TimeZone = 'America/Havana';
SELECT '2027-01-15 12:00Z'::timestamptz, '2026-12-01 12:00Z'::timestamptz, '2027-07-01 12:00Z'::timestamptz;
SET TimeZone = 'UTC';
SELECT count(*) FROM pg_timezone_names;
