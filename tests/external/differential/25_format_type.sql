-- `format_type(oid, typmod)` renders a type as it was declared. The modifier
-- was parsed and discarded, so every column came back as an unconstrained type;
-- tools that introspect a schema read the answer literally.
--
-- The modifier is always taken from `pg_attribute` rather than written as a
-- literal: the two engines encode the temporal precisions differently (B-140),
-- so the same integer does not mean the same thing to both, while each
-- engine's own stored value does.

CREATE TABLE fmt (
  a varchar(5),
  b char(3),
  c numeric(6,2),
  d timestamp(3),
  e int,
  f time(2),
  g text,
  h varchar,
  i numeric
);

SELECT format_type(atttypid, atttypmod) FROM pg_attribute
 WHERE attrelid = 'fmt'::regclass AND attnum > 0 ORDER BY attnum;

SELECT attname, format_type(atttypid, atttypmod) FROM pg_attribute
 WHERE attrelid = 'fmt'::regclass AND attnum > 0 ORDER BY attname;

-- a type with no modifier renders bare, whatever is passed for one
SELECT format_type(23, -1);
SELECT format_type(25, -1);
SELECT format_type(16, -1);
SELECT format_type(1043, -1);
SELECT format_type(1700, -1);
SELECT format_type(1186, -1);

-- and the declared modifiers still take effect on values
SELECT '12:34:56.789123'::time(2);
SELECT '2020-01-01 00:00:00.123456'::timestamp(3);
SELECT '1.234567 sec'::interval(1);
SELECT 'abcdefg'::varchar(5);
SELECT 1.005::numeric(6,2);

DROP TABLE fmt;
