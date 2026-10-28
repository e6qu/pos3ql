SET timezone = 'UTC';

SELECT to_timestamp('2021-06-01 12:00 +03:15 EST 45', 'YYYY-MM-DD HH24:MI TZH:TZM TZ TZM');

SELECT to_timestamp('2021-06-01 12:00 +03 EST 04', 'YYYY-MM-DD HH24:MI TZH TZ TZH');

SELECT to_timestamp('2021-06-01 12:00 NoZone', 'YYYY-MM-DD HH24:MI TZ');

SELECT to_timestamp('2021-06-01 12:00 +03 EST 04', 'YYYY-MM-DD HH24:MI tzh tz tzh');
