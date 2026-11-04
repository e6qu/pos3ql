SELECT percentile_cont(0.5) WITHIN GROUP(ORDER BY x),
       pg_typeof(percentile_cont(0.5) WITHIN GROUP(ORDER BY x)),
       encode(float8send(percentile_cont(0.5) WITHIN GROUP(ORDER BY x)),'hex'),
       percentile_disc(0.5) WITHIN GROUP(ORDER BY x),mode() WITHIN GROUP(ORDER BY x)
FROM(VALUES(1.0),(3.0))v(x);
SELECT percentile_cont(0.5) WITHIN GROUP(ORDER BY x),
       pg_typeof(percentile_cont(0.5) WITHIN GROUP(ORDER BY x))
FROM(VALUES(INTERVAL'1 month 1 day 1 microsecond'),(INTERVAL'2 months 2 days 4 microseconds'))v(x);
SELECT percentile_cont('[0:3]={0,0.5,1,NULL}'::float8[]) WITHIN GROUP(ORDER BY x),
       pg_typeof(percentile_cont(ARRAY[0.5]) WITHIN GROUP(ORDER BY x)),
       percentile_disc(ARRAY[0,0.5,1,NULL]) WITHIN GROUP(ORDER BY x)
FROM(VALUES(1.0),(3.0))v(x);
SELECT percentile_cont(ARRAY[0,0.5,1,NULL]) WITHIN GROUP(ORDER BY x)
FROM(VALUES(INTERVAL'1 day'),(INTERVAL'3 days'))v(x);
SELECT percentile_cont(NULL::float8) WITHIN GROUP(ORDER BY x),
       percentile_cont(NULL::float8[]) WITHIN GROUP(ORDER BY x),
       percentile_cont(ARRAY[]::float8[]) WITHIN GROUP(ORDER BY x)
FROM(VALUES(1.0))v(x);
SELECT percentile_cont(ARRAY[0.5]) WITHIN GROUP(ORDER BY x) FROM(VALUES(1.0))v(x) WHERE false;
SELECT percentile_cont(ARRAY[0,1]) WITHIN GROUP(ORDER BY x DESC),
       percentile_disc(0) WITHIN GROUP(ORDER BY x DESC),mode() WITHIN GROUP(ORDER BY x DESC)
FROM(VALUES(1.0),(3.0))v(x);
SELECT fraction,percentile_cont(fraction) WITHIN GROUP(ORDER BY value)
FROM(VALUES(0,1),(0,3),(1,1),(1,3))inputs(fraction,value) GROUP BY fraction ORDER BY fraction;
CREATE SEQUENCE percentile_direct_calls;
SELECT percentile_cont((nextval('percentile_direct_calls')-1)::float8) WITHIN GROUP(ORDER BY x)
FROM(VALUES(1),(3))v(x);
SELECT nextval('percentile_direct_calls');
-- next request
SELECT percentile_cont(-0.1) WITHIN GROUP(ORDER BY x) FROM(VALUES(1.0))v(x);
-- next request
SELECT percentile_cont(-0.1) WITHIN GROUP(ORDER BY x) FROM(VALUES(1.0))v(x) WHERE false;
-- next request
SELECT percentile_cont(ARRAY[-0.1]) WITHIN GROUP(ORDER BY x) FROM(VALUES(1.0))v(x);
-- next request
SELECT percentile_cont(0.5) WITHIN GROUP(ORDER BY x)
FROM(VALUES(INTERVAL'-1 month'),(INTERVAL'2147483647 months'))v(x);
-- next request
DROP SEQUENCE percentile_direct_calls;
