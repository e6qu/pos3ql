-- `INTERVAL '1-2' YEAR TO MONTH`: the hyphenated value is years and months, a
-- bare number is months (the trailing field), and a leading sign applies to
-- both. The month part must be under twelve, since twelve months is a year;
-- PostgreSQL rejects a larger one as out of range, and a non-numeric part as
-- invalid syntax.

SELECT INTERVAL '1-2' YEAR TO MONTH;
SELECT INTERVAL '1' YEAR TO MONTH;
SELECT INTERVAL '5' YEAR TO MONTH;
SELECT INTERVAL '0-5' YEAR TO MONTH;
SELECT INTERVAL '2-0' YEAR TO MONTH;
SELECT INTERVAL '10-11' YEAR TO MONTH;
SELECT INTERVAL '-1-2' YEAR TO MONTH;
SELECT INTERVAL '-0-6' YEAR TO MONTH;

-- composes with arithmetic and a text cast
SELECT (INTERVAL '1-2' YEAR TO MONTH)::text;
SELECT INTERVAL '1-2' YEAR TO MONTH + INTERVAL '1' MONTH;
SELECT INTERVAL '2-6' YEAR TO MONTH * 2;

-- a month of twelve or more carries into the year field, which the two-field
-- form does not allow; a non-numeric part is a syntax error
SELECT INTERVAL '1-13' YEAR TO MONTH;
SELECT INTERVAL 'x-2' YEAR TO MONTH;
SELECT INTERVAL '1-x' YEAR TO MONTH;
SELECT INTERVAL 'abc' YEAR TO MONTH;

-- the single-field qualifier and bare-number forms are unaffected
SELECT INTERVAL '1' YEAR;
SELECT INTERVAL '2' MONTH;
SELECT INTERVAL '1' DAY;
SELECT INTERVAL '90' MINUTE;
SELECT INTERVAL '1 day';
