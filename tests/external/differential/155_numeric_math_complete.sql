-- PostgreSQL 18 mathematical completion: exact degree landmarks, special
-- functions, ranged and normal randomness, expression storage, and catalogs.
DROP TABLE IF EXISTS numeric_math_complete;

SELECT sind(0), sind(30), sind(90), sind(180), sind(270), sind(360),
       cosd(0), cosd(60), cosd(90), cosd(180), cosd(360);
SELECT tand(0), tand(45), tand(90), tand(180),
       cotd(0), cotd(45), cotd(90), cotd(180);
SELECT asind(-1), asind(-.5), asind(0), asind(.5), asind(1),
       acosd(-1), acosd(-.5), acosd(0), acosd(.5), acosd(1),
       atand('-Infinity'::float8), atand(-1), atand(0), atand(1),
       atand('Infinity'::float8);
SELECT atan2d(1,1), atan2d(1,-1), atan2d(-1,-1), atan2d(-1,1),
       atan2d(0,-1);

SELECT erf('-Infinity'::float8), erf(0), erf('Infinity'::float8),
       erfc('-Infinity'::float8), erfc(0), erfc('Infinity'::float8),
       gamma(1), gamma(5), lgamma(1);
-- Finite C-library results can vary across hosts. Check their error explicitly;
-- keep exact landmarks, exceptional values and SQL errors as exact comparisons.
-- https://www.postgresql.org/docs/18/functions-math.html
SELECT check_name,
       abs(actual - expected) <=
         4 * 2.220446049250313e-16::float8 * greatest(1::float8, abs(expected))
FROM (VALUES
  ('erf(-1)', erf(-1), -0.8427007929497149::float8),
  ('erf(1)', erf(1), 0.8427007929497149::float8),
  ('erfc(-1)', erfc(-1), 1.842700792949715::float8),
  ('erfc(1)', erfc(1), 0.15729920705028513::float8),
  ('gamma(.5)', gamma(.5), 1.772453850905516::float8),
  ('gamma(5.5)', gamma(5.5), 52.34277778455352::float8),
  ('lgamma(.5)', lgamma(.5), 0.5723649429247001::float8),
  ('lgamma(5)', lgamma(5), 3.1780538303479458::float8),
  ('lgamma(5.5)', lgamma(5.5), 3.9578139676187165::float8)
) AS checks(check_name, actual, expected)
ORDER BY check_name;

SELECT setseed(0.5);
SELECT random(), random();
SELECT setseed(0.5);
SELECT random(1,10), random(-10,10), pg_typeof(random(1,10));
SELECT setseed(0.5);
SELECT random(-9000000000::bigint,9000000000::bigint),
       pg_typeof(random(1::bigint,10::bigint));
SELECT setseed(0.5);
SELECT random(1.20::numeric,2.40::numeric),
       random(-1000.001::numeric,1000.001::numeric),
       pg_typeof(random(1.20::numeric,2.40::numeric));
SELECT random(2.20::numeric,2.20::numeric),
       scale(random(2.20::numeric,2.20::numeric));
SELECT setseed(0.5);
SELECT random_normal(), random_normal(10), random_normal(10,2),
       pg_typeof(random_normal());

SELECT asin(2), acos(-2), acosh(0), atanh(2);
SELECT sind('Infinity'::float8), cosd('-Infinity'::float8);
SELECT gamma(0), gamma(-1), gamma('-Infinity'::float8);
SELECT random(10,1);
SELECT random(2.0::numeric,1.0::numeric);

CREATE TABLE numeric_math_complete (
  id integer PRIMARY KEY,
  angle double precision NOT NULL,
  sine double precision GENERATED ALWAYS AS (sind(angle)) STORED,
  error_value double precision GENERATED ALWAYS AS (erf(angle / 90.0)) STORED
);
CREATE INDEX numeric_math_sine_idx ON numeric_math_complete (sind(angle));
INSERT INTO numeric_math_complete (id, angle) VALUES
  (1, 0), (2, 30), (3, 90), (4, 180), (5, 270);
-- This output checks saved/generated values to twelve decimal places. The
-- direct special-function checks above retain a much tighter error bound.
SELECT id, angle, sine, round(error_value::numeric, 12)
FROM numeric_math_complete ORDER BY sind(angle), id;
UPDATE numeric_math_complete SET angle = angle + 30 WHERE id IN (1,2);
SELECT id, sine FROM numeric_math_complete
WHERE sind(angle) >= .5 ORDER BY id;

SELECT oid, proname, prorettype, proargtypes, pronargdefaults,
       provolatile, proparallel, prosrc
FROM pg_proc
WHERE oid IN (320,940,941,947,1194,1340,1341,1342,1343,1344,
              1345,1346,1347,1368,1376,1394,1395,1396,1397,1398,
              1481,1598,1599,1600,1601,1602,1603,1604,1605,1606,
              1607,1608,1609,1610,1705,1706,1707,1708,1709,1710,
              1711,1712,1728)
ORDER BY oid;
SELECT oid, proname, prorettype, proargtypes, pronargdefaults,
       provolatile, proparallel, prosrc
FROM pg_proc
WHERE oid IN (1730,1732,1734,1736,1738,1741,1973,
              2167,2169,2170,2308,2309,2310,2320,2462,2463,2464,
              2465,2466,2467,2731,2732,2733,2734,2735,2736,2737,
              2738,3218,3281,5042,5043,5044,5045,5046,5047,5048,
              5049,6212,6219,6220,6339,6340,6341,6383,6384)
ORDER BY oid;
SELECT pg_get_function_arguments(6212), pg_get_function_result(6212),
       pg_get_function_arguments(6341);

DROP TABLE numeric_math_complete;
