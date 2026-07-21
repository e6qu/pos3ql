-- Dates, timestamps, uuid, bytea.
SET timezone TO 'UTC';
CREATE TABLE ev7 (id int, d date, t timestamptz, u uuid, raw bytea);
INSERT INTO ev7 VALUES
  (1, '2024-02-29', '2024-02-29 12:30:45.5+00', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '\xdeadbeef'),
  (2, '2000-01-01', '2024-03-01T00:00:00+02',   'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A99', '\x00ff');
SELECT * FROM ev7 ORDER BY id;
SELECT id FROM ev7 WHERE d > '2020-01-01' ORDER BY id;
SELECT id FROM ev7 WHERE t BETWEEN '2024-02-01' AND '2024-02-29 23:59:59Z' ORDER BY id;
SELECT '2024-06-15'::date, '2024-06-15 10:00'::timestamp;
SELECT 'b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, '\x0102'::bytea;
SELECT max(t), min(d) FROM ev7;
SELECT id FROM ev7 WHERE u = 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11';
SELECT '2023-02-29'::date;
DROP TABLE ev7;
-- interval scaling and justification
SELECT interval '1 day' * 3, interval '1 month' * 1.5, interval '2 hours' * 2.5;
SELECT interval '1 day' / 2, interval '10 days' / 3, 2 * interval '3 hours';
SELECT justify_hours(interval '36 hours'), justify_days(interval '35 days');
SELECT justify_interval(interval '1 mon -1 hour');
-- age (symbolic calendar interval)
SELECT age(timestamp '2024-06-15', timestamp '2020-01-10');
SELECT age(timestamp '2020-01-10', timestamp '2024-06-15');
SELECT age(timestamp '2024-03-01', timestamp '2024-01-31');
SELECT age(timestamp '2024-01-01 10:00', timestamp '2023-12-15 14:30');
SELECT age(date '2000-01-01', date '1999-02-05');
-- to_timestamp(epoch): Unix seconds -> timestamptz
SELECT to_timestamp(0), to_timestamp(1700000000), to_timestamp(1700000000.5), to_timestamp(-100000);
-- AT TIME ZONE (timestamp <-> timestamptz), with DST and offset zones
SELECT timestamp '2024-01-01 12:00' AT TIME ZONE 'UTC';
SELECT timestamptz '2024-01-01 12:00+00' AT TIME ZONE 'America/New_York';
SELECT timestamp '2024-07-01 12:00' AT TIME ZONE 'America/New_York';
SELECT timestamp '2024-06-01 12:00' AT TIME ZONE '+05';
SELECT timestamptz '2024-06-01 12:00+00' AT TIME ZONE 'Etc/GMT-3';
SELECT timezone('UTC', timestamptz '2024-01-01 00:00+00');
-- make_interval: positional and named-argument field composition
SELECT make_interval(1, 2, 3, 4, 5, 6, 7.5);
SELECT make_interval(years => 2, months => 6);
SELECT make_interval(days => 10, hours => 3, mins => 30, secs => 15.25);
SELECT make_interval(weeks => 2);
SELECT make_interval(secs => 90.5);
SELECT make_interval();
SELECT make_interval(mins => -90);
SELECT make_interval(1, 2) + make_interval(hours => 5);
-- fractional-second precision typmods; prepared-parameter typmods
SELECT '2024-01-01 12:34:56.123456'::timestamp(3);
SELECT '2024-01-01 12:34:56.1235'::timestamp(3);
SELECT '2024-01-01 12:34:56.1225'::timestamp(3);
SELECT '12:34:56.789'::time(1);
SELECT '1 day 12:34:56.789'::interval(2);
SELECT '2024-01-01 12:34:56.9999'::timestamp(0);
SELECT '2024-01-01 12:34:56.123456+00'::timestamptz(2);
PREPARE tmq (varchar(10)) AS SELECT $1;
EXECUTE tmq('hello');
PREPARE tmq2 (varchar(2)) AS SELECT $1;
EXECUTE tmq2('hello');
PREPARE tmq3 (numeric(4,1)) AS SELECT $1;
EXECUTE tmq3(12.345);
CREATE TABLE tmt (a timestamp(2), b time(1), c interval(0));
INSERT INTO tmt VALUES ('2024-01-01 00:00:00.126','01:02:03.45','5 minutes 3.6 seconds') RETURNING *;
DROP TABLE tmt;
-- fractional-second precision above 6 is clamped, with PostgreSQL's warning
SELECT '2020-01-01 12:34:56'::timestamp(7);
SELECT '12:00:00'::time(7);
SELECT '2020-01-01'::timestamptz(7);
-- (timetz is not implemented yet — B-092)
SELECT '1 day'::interval(7);
SELECT '2020-01-01'::timestamp(7), '12:00'::time(8);
-- in range: no warning
SELECT '2020-01-01'::timestamp(6);
SELECT '2020-01-01'::timestamp(0);
-- timetz (time with time zone)
SELECT '12:00:00'::timetz, '12:00:00+05'::timetz, '12:00:00-05:30'::timetz;
SELECT '12:00:00.123456+02:00'::timetz, '12:00:00.1+02:00'::timetz, '12:00:00+05:45'::timetz;
SELECT '24:00:00'::timetz, '12:00:00 UTC'::timetz;
SELECT pg_typeof('12:00:00+05'::timetz), '12:00:00+05'::timetz::text;
-- casts in both directions; a zoneless source takes the session zone
SELECT '12:00:00'::time::timetz, '12:00:00+05'::timetz::time;
SELECT '2020-06-15 12:00:00+00'::timestamptz::timetz;
-- precision is clamped and rounded like the other temporal types
SELECT '12:00:00.123456+02'::timetz(2), '12:00:00.125+02'::timetz(2), '12:00:00.987654+02'::timetz(0);
-- ordering is by the instant, then by zone, so equal instants in different
-- zones are ordered but never equal
SELECT ('12:00:00+00'::timetz = '13:00:00+01'::timetz), ('12:00:00+00'::timetz > '13:00:00+01'::timetz);
SELECT ('12:00:00+00'::timetz < '12:00:00+01'::timetz), ('12:00:00+00'::timetz = '12:00:00+00'::timetz);
SELECT v.x::text FROM (VALUES ('12:00:00+00'::timetz),('12:00:00+01'),('12:00:00-01'),('13:00:00+01')) v(x) ORDER BY v.x;
SELECT count(*) FROM (SELECT DISTINCT v.x FROM (VALUES ('12:00:00+00'::timetz),('13:00:00+01')) v(x)) q;
-- interval arithmetic wraps within the day and keeps the zone
SELECT ('12:00:00+05'::timetz + '1 hour'::interval)::text, ('12:00:00+05'::timetz - '1 hour'::interval)::text;
SELECT ('23:30:00+00'::timetz + '1 hour'::interval)::text;
SELECT ('12:00:00'::time + '1 hour'::interval)::text, ('00:30:00'::time - '1 hour'::interval)::text;
SELECT ('1 hour'::interval + '12:00:00'::time)::text, ('12:00:00'::time + '90 minutes'::interval)::text;
-- extract
SELECT extract(hour FROM '12:34:56+05'::timetz), extract(minute FROM '12:34:56+05'::timetz);
SELECT extract(second FROM '12:34:56+05'::timetz), extract(timezone FROM '12:00:00+05'::timetz);
SELECT extract(timezone_hour FROM '12:00:00+05:30'::timetz), extract(hour FROM '12:34:56'::time);
SELECT date_part('minute', '12:34:00+05'::timetz);
-- stored, compared against a bare literal, aggregated
CREATE TABLE tzt(a timetz);
INSERT INTO tzt VALUES ('12:00:00+05'),('08:30:00-03:30'),('00:00:00+00');
SELECT a::text FROM tzt ORDER BY a;
SELECT a::text FROM tzt WHERE a > '00:00:00+00' ORDER BY a;
SELECT max(a)::text, min(a)::text, count(DISTINCT a) FROM tzt;
DROP TABLE tzt;
-- a zone suffix on a plain `time` is accepted and ignored, as PostgreSQL does
SELECT '12:00:00-05'::time, '12:00:00+05'::time, '12:00:00Z'::time;
CREATE TABLE tmt(a time);
INSERT INTO tmt VALUES ('12:00:00');
SELECT a FROM tmt WHERE a > '01:00:00';
SELECT a FROM tmt WHERE a = '12:00:00';
DROP TABLE tmt;
-- the SQL-standard functions written without parentheses. Their values move
-- with the clock, so the probes assert type and shape; a keyword-classification
-- change once made every one of these a syntax error with nothing to catch it.
SELECT pg_typeof(current_date), pg_typeof(current_timestamp), pg_typeof(localtimestamp);
SELECT pg_typeof(current_time), pg_typeof(localtime);
SELECT pg_typeof(current_time(0)), pg_typeof(localtime(3)), pg_typeof(current_timestamp(0));
SELECT current_date <= current_date, current_timestamp <= clock_timestamp();
SELECT current_time::text ~ '^[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?[+-][0-9]{2}(:[0-9]{2})?$';
SELECT localtime::text ~ '^[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?$';
SELECT current_time(0)::text ~ '^[0-9]{2}:[0-9]{2}:[0-9]{2}[+-][0-9]{2}(:[0-9]{2})?$';
SELECT localtime(0)::text ~ '^[0-9]{2}:[0-9]{2}:[0-9]{2}$';
SELECT date_part('hour', localtime) = date_part('hour', current_time);
SET TimeZone='Asia/Kolkata';
SELECT right(current_time::text, 6);
-- (localtime = current_time::time is racy here: each reads the clock
--  separately, where PostgreSQL stabilizes both — B-101)
SET TimeZone='UTC';
SELECT right(current_time::text, 3);
