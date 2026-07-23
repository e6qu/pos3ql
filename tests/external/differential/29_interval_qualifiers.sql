-- The SQL-standard interval unit qualifier `INTERVAL '1' DAY`, and a bare
-- numeric interval string, both of which PostgreSQL accepts. A single field
-- interprets the value in that unit and truncates toward zero to the field's
-- resolution for every field but SECOND, which keeps its fraction. A bare
-- number with no unit is seconds. A word that is not an interval field is the
-- output column's alias, not a qualifier.

SELECT INTERVAL '1' DAY;
SELECT INTERVAL '1' HOUR;
SELECT INTERVAL '2' MONTH;
SELECT INTERVAL '1' YEAR;
SELECT INTERVAL '90' MINUTE;
SELECT INTERVAL '5' SECOND;

-- truncation toward zero for coarse fields; SECOND keeps its fraction
SELECT INTERVAL '2.5' HOUR;
SELECT INTERVAL '2.5' DAY;
SELECT INTERVAL '90.7' MINUTE;
SELECT INTERVAL '1.5' YEAR;
SELECT INTERVAL '5.9' SECOND;
SELECT INTERVAL '-2.5' HOUR;
SELECT INTERVAL '1.9' DAY;

-- qualifier composes with arithmetic and casts
SELECT INTERVAL '1 day' + INTERVAL '2' HOUR;
SELECT INTERVAL '3' DAY * 2;
SELECT (INTERVAL '1' DAY)::text;

-- a bare number is seconds
SELECT INTERVAL '1';
SELECT INTERVAL '90';
SELECT INTERVAL '1.5';
SELECT INTERVAL '-5';

-- WEEK is not a standard field qualifier, so it is the column alias over a bare
-- one-second interval
SELECT INTERVAL '1' WEEK;

-- the all-in-the-string form is unchanged
SELECT INTERVAL '1 day';
SELECT INTERVAL '2 hours 30 minutes';
SELECT INTERVAL '1 day 90';
