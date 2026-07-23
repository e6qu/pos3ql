-- smallint as a real runtime type, and the widened array element set.
-- Each probe diverged before its fix.
CREATE TABLE i2 (s smallint);
INSERT INTO i2 VALUES (32767), (5), (-32768);

-- The type survives expressions; arithmetic keeps the width and its bounds.
SELECT pg_typeof(s), pg_typeof(s + s), pg_typeof(s + 1), pg_typeof(s + 1::int8) FROM i2 WHERE s = 5;
SELECT pg_typeof(-s), pg_typeof(abs(s)), pg_typeof(s / 2::int2), pg_typeof(s % 2::int2) FROM i2 WHERE s = 5;
SELECT 32767::int2 + 1::int2;
SELECT (-32768::int2) - 1::int2;
SELECT -32768::int2;
SELECT (-32768)::int2;
SELECT 16383::int2 * 2::int2, 16384::int2 * 2::int2;
SELECT '12'::int2, ' 12 '::int2;
SELECT '40000'::int2;
SELECT 40000::int2;

-- Bit operators: shifts keep the left type and truncate silently.
SELECT 1::int2 << 15, pg_typeof(1::int2 << 15), 16000::int2 << 2;
SELECT pg_typeof(1::int2 & 3::int2), pg_typeof(1::int2 & 3), pg_typeof(~1::int2), ~1::int2;

-- Aggregates.
SELECT sum(s), pg_typeof(sum(s)), avg(s), pg_typeof(avg(s)) FROM i2;
SELECT min(s), max(s), pg_typeof(min(s)), count(s) FROM i2;
SELECT stddev(s), variance(s) FROM i2 WHERE s > 0;
SELECT mod(7::int2, 3::int2), pg_typeof(mod(7::int2, 3::int2));
SELECT gcd(6::int2, 4::int2);
SELECT lcm(6::int2, 4::int2);

-- Comparisons across the tower; ordering; grouping.
SELECT s = 5, s = 5::int8, s < 5.5, s = 5::numeric, s <> 4::int2 FROM i2 WHERE s = 5;
SELECT s FROM i2 ORDER BY s;
SELECT s, count(*) FROM i2 GROUP BY s ORDER BY s;
SELECT DISTINCT s FROM i2 ORDER BY s;

-- The wire type is honest.
SELECT pg_typeof(1::int2 * 2::int2), pg_typeof(2::int2 ^ 2::int2);
SELECT floor(5::int2), sign(-3::int2), round(7::int2), ceil(5::int2), trunc(5::int2);
SELECT to_hex(255::int2);
SELECT greatest(1::int2, 2::int2), least(1::int2, 2::int8), coalesce(NULL::int2, 3::int2), nullif(5::int2, 5::int2) IS NULL;
SELECT abs(-32768::int2);

-- smallserial keeps counting in smallint.
CREATE TABLE ss (id smallserial, v int);
INSERT INTO ss(v) VALUES (1), (2);
SELECT id, pg_typeof(id) FROM ss ORDER BY id;
DROP TABLE ss;
DROP TABLE i2;

-- Array elements beyond the original nine.
CREATE TABLE ar (t time, z timetz, i interval, u uuid, b bytea, j jsonb, sj json, c char(3), v varchar(5), s smallint);
INSERT INTO ar VALUES ('12:00','13:00+02','1 day','a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11','\xdeadbeef','{"k":1}','{"a" :2}','ab','hey',7);
SELECT array_agg(t), array_agg(z), array_agg(i), pg_typeof(array_agg(t)) FROM ar;
SELECT array_agg(u), array_agg(b), array_agg(j), array_agg(sj) FROM ar;
SELECT array_agg(c), array_agg(v), array_agg(s) FROM ar;
SELECT pg_typeof(array_agg(c)), pg_typeof(array_agg(v)), pg_typeof(array_agg(s)), pg_typeof(array_agg(j)) FROM ar;
SELECT ARRAY['12:00'::time, '13:30'::time], ('{12:00,13:30}'::time[])[2];
SELECT ARRAY['1 day'::interval, '2 hours'::interval];
SELECT ARRAY['a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid];
SELECT ARRAY['\xdead'::bytea], ARRAY['{"x":1}'::jsonb];
SELECT pg_typeof(ARRAY[1::int2]), ARRAY[1::int2, 300::int2];
SELECT ('{1,2}'::int2[])[2], pg_typeof(('{1,2}'::int2[])[2]);
SELECT '{40000}'::int2[];
SELECT unnest('{12:00,13:30}'::time[]) ORDER BY 1;
SELECT unnest('{1 day,2 days}'::interval[]) ORDER BY 1;
SELECT array_length('{1 day,2 days}'::interval[], 1), array_length('{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}'::uuid[], 1);
SELECT '{12:00,13:30}'::time[] = '{12:00,13:30}'::time[], '{1,2}'::int2[] < '{1,3}'::int2[];
SELECT t = t, u = u FROM (SELECT array_agg(t) AS t, array_agg(u) AS u FROM ar) x;
DROP TABLE ar;
