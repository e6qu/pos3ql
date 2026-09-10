-- PostgreSQL 18 temporal values, operators, casts, support routines, and
-- aggregate catalogs are one connected compatibility boundary.
SET TIME ZONE 'America/New_York';

SELECT date 'infinity', date '-infinity', isfinite(date 'infinity'),
       isfinite(timestamp 'infinity'), isfinite(interval '1 day');
SELECT interval 'infinity',interval '-infinity',isfinite(interval 'infinity'),
       interval 'infinity'+interval '1 day',interval 'infinity' * -2,
       interval '1 day' * 'Infinity'::float8,
       interval '1 day' / 'Infinity'::float8,
       age(timestamp 'infinity',timestamp '2000-01-01');
SELECT date_bin(interval '1 day',timestamp 'infinity',timestamp '2000-01-01'),
       date_trunc('day',timestamp '-infinity'),
       to_char(timestamp 'infinity','YYYY') IS NULL;
SELECT extract(epoch from time '01:02:03.5'),
       extract(milliseconds from time '01:02:03.5'),
       extract(julian from date '2000-01-01'),
       extract(julian from timestamp '2000-01-01'),
       extract(julian from timestamp '2000-01-01 12:00'),
       extract(julian from timestamp '1999-12-31 23:59:59.123456'),
       extract(epoch from interval 'infinity'),
       date_part('month',date 'infinity') IS NULL;
SELECT numeric 'Infinity'+1,numeric '-Infinity' * -2,
       numeric 'Infinity'+numeric '-Infinity',numeric '5'%numeric 'Infinity';
SELECT abs(numeric '-Infinity'),sign(numeric '-Infinity'),
       sqrt(numeric 'Infinity'),ln(numeric 'Infinity'),
       exp(numeric '-Infinity'),scale(numeric 'Infinity') IS NULL,
       min_scale(numeric 'Infinity') IS NULL,trim_scale(numeric 'Infinity'),
       power(numeric '-Infinity',3),power(0::numeric,numeric 'Infinity'),
       power(0.5::numeric,numeric '-Infinity'),div(numeric 'Infinity',2);
SELECT to_char(numeric 'Infinity','S999.99'),
       to_char(numeric '-Infinity','S999.99'),
       to_char(numeric 'Infinity','9.9EEEE'),
       numrange(numeric 'Infinity',numeric 'Infinity','[]'),
       hash_range(numrange(numeric '-Infinity',numeric 'Infinity','[]'));
SELECT to_json(numeric 'Infinity'),to_json(numeric '-Infinity'),
       to_json(numeric 'NaN'),json_build_array(numeric 'Infinity'),
       to_json('Infinity'::float8);
SELECT daterange(date 'infinity',date 'infinity','[]'),
       daterange(date '-infinity',date '-infinity','[]'),
       daterange(date '2024-01-01',date 'infinity','[]'),
       daterange(date '-infinity',date '2024-01-01','(]');
SELECT timestamp '2024-07-15 12:00'::timestamptz,
       timestamptz '2024-07-15 16:00+00'::timestamp,
       timestamptz '2024-07-15 02:00+00'::date,
       timestamptz '2024-07-15 16:30+00'::time,
       interval '27:04:05'::time;
SELECT timestamp '2024-07-15 12:00' = timestamptz '2024-07-15 16:00+00',
       date '2024-07-15' = timestamp '2024-07-15 00:00',
       date '2024-07-15' = timestamptz '2024-07-15 04:00+00',
       date_cmp_timestamp(date '2024-07-15',timestamp '2024-07-16'),
       timestamptz_cmp_date(timestamptz '2024-07-15 03:59:59+00',date '2024-07-15');

SELECT timestamp '2024-07-15 12:00' AT LOCAL,
       timestamptz '2024-07-15 16:00+00' AT LOCAL,
       timetz '12:00+02' AT LOCAL;
SELECT timezone(interval '2 hours',timestamp '2024-07-15 12:00'),
       timezone(interval '2 hours',timestamptz '2024-07-15 12:00+00'),
       timezone('UTC',timetz '12:00+02');
SELECT make_timestamptz(2024,7,15,12,0,0,'Europe/Bucharest'),
       pg_catalog.timestamp(date '2024-01-02',time '03:04:05'),
       pg_catalog.timestamptz(date '2024-01-02',time '03:04:05'),
       pg_catalog.timestamptz(date '2024-01-02',timetz '03:04:05+02'),
       pg_catalog.interval(time '03:04:05'),
       pg_catalog.timestamp(timestamp '2024-01-01 00:00:00.555555',3);

SELECT date_add(timestamptz '2024-03-09 12:00-05',interval '1 day'),
       date_add(timestamptz '2024-03-09 12:00-05',interval '1 day','UTC'),
       date_subtract(timestamptz '2024-03-11 12:00-04',interval '1 day'),
       date_add(timestamp '2024-03-09 12:00','1 day'),
       date_add(date '2024-03-09',interval '1 day'),
       date_subtract(timestamp '2024-03-11 12:00',interval '1 day','UTC');
SELECT date_trunc('hour',timestamptz '2024-07-15 12:34:56-04','UTC'),
       date_trunc('day',timestamp '2024-07-15 12:34:56','UTC'),
       date_trunc('day',date '2024-07-15','UTC'),
       date_trunc('milliseconds',timestamp '2024-01-01 00:00:01.234567'),
       date_trunc('decade',interval '123 years 7 mons 8 days 09:10:11.654321');
SELECT date '2024-01-02' + time '03:04:05',
       time '03:04:05' + date '2024-01-02',
       date '2024-01-02' + timetz '03:04:05+02',
       interval '2 hours' + timetz '03:04:05+02',
       time '03:04:05' - time '01:02:03';

SELECT hashdate(date '2024-01-02'),hashdateextended(date '2024-01-02',123),
       time_hash(time '03:04:05'),timetz_hash(timetz '03:04:05+02'),
       timestamp_hash(timestamp '2024-01-02 03:04:05'),
       timestamptz_hash(timestamptz '2024-01-02 03:04:05+02'),
       interval_hash(interval '1 month'),interval_hash(interval '30 days'),
       hashdate(date 'infinity'),hashdate(date '-infinity'),
       timestamp_hash(timestamp 'infinity'),timestamp_hash(timestamp '-infinity'),
       interval_hash(interval 'infinity'),interval_hash(interval '-infinity'),
       interval_hash_extended(interval 'infinity',123),
       interval_hash_extended(interval '-infinity',123);

CREATE TABLE temporal_complete_values (
  d date, t time, tz timetz, ts timestamp, tstz timestamptz, iv interval
);
INSERT INTO temporal_complete_values VALUES
  ('2024-01-02','03:04:05','03:04:05+02','2024-01-02 03:04:05',
   '2024-01-02 03:04:05+02','1 month'),
  ('2023-12-31','23:59:59','23:59:59-05','2023-12-31 23:59:59',
   '2023-12-31 23:59:59-05','30 days'),
  (NULL,NULL,NULL,NULL,NULL,NULL);
SELECT min(d),max(d),min(t),max(t),min(tz),max(tz),min(ts),max(ts),
       min(tstz),max(tstz),min(iv),max(iv),sum(iv),avg(iv)
  FROM temporal_complete_values;

SELECT count(*) FROM pg_operator WHERE oprnamespace=11 AND
       (oprleft IN (1082,1083,1114,1184,1186,1266) OR
        oprright IN (1082,1083,1114,1184,1186,1266));
SELECT count(*) FROM pg_cast WHERE oid BETWEEN 10158 AND 10170 OR
                                      oid BETWEEN 10212 AND 10216;
SELECT oid,proname,prosrc,proisstrict,provolatile,proparallel,
       pronargdefaults,proargnames
  FROM pg_proc WHERE oid IN (1026,1271,3463,3464,6222,6334)
 ORDER BY oid;
SELECT aggfnoid::oid,aggtransfn::oid,aggfinalfn::oid,aggcombinefn::oid,
       aggsortop,aggtranstype
  FROM pg_aggregate WHERE aggfnoid IN (2106,2113,2122,2144)
 ORDER BY aggfnoid;
