-- The clock `TO`-range interval qualifiers, and mixed-sign interval rendering.
-- A range's value is a day count (only when it starts at DAY) followed by an
-- H:M:S clock, truncated to the trailing field; a two-part clock is H:M or M:S
-- per the leading field, a three-part clock is always H:M:S, and a bare number
-- takes the trailing field. Day and clock carry independent signs. On output, a
-- positive field takes an explicit `+` only when the field before it was
-- negative.

-- DAY-leading ranges
SELECT INTERVAL '1 2:03:04' DAY TO SECOND;
SELECT INTERVAL '1 2:03' DAY TO MINUTE;
SELECT INTERVAL '1 2' DAY TO HOUR;
SELECT INTERVAL '1 2:03:04' DAY TO HOUR;
SELECT INTERVAL '1 2:03:04.5' DAY TO MINUTE;
SELECT INTERVAL '1 25:00' DAY TO HOUR;
SELECT INTERVAL '-1 2:03' DAY TO MINUTE;
SELECT INTERVAL '1 -2:03' DAY TO MINUTE;

-- time-leading ranges
SELECT INTERVAL '2:03:04' HOUR TO SECOND;
SELECT INTERVAL '2:03' HOUR TO MINUTE;
SELECT INTERVAL '2:03:04' HOUR TO MINUTE;
SELECT INTERVAL '3:04' MINUTE TO SECOND;
SELECT INTERVAL '2:03:04.5' MINUTE TO SECOND;

-- a bare number takes the trailing field
SELECT INTERVAL '5' DAY TO HOUR;
SELECT INTERVAL '100' MINUTE TO SECOND;

-- malformed values and invalid field orderings error
SELECT INTERVAL 'bad' HOUR TO SECOND;
SELECT INTERVAL '1 x:03' DAY TO MINUTE;

-- mixed-sign rendering: a `+` appears only after a negative field
SELECT INTERVAL '-1 day 2 hours';
SELECT INTERVAL '1 day -2 hours';
SELECT INTERVAL '-1 month 5 days';
SELECT INTERVAL '-2 days -3 hours';
SELECT INTERVAL '1 year 2 mons -3 days';
SELECT INTERVAL '-1 year 2 mons 3 days';
SELECT INTERVAL '1 mon -3 days 4 hours';
SELECT INTERVAL '1 year 2 mons 3 days 04:05:06';
SELECT INTERVAL '1 day 2 hours';
SELECT INTERVAL '-1 day';
SELECT INTERVAL '2 hours';
