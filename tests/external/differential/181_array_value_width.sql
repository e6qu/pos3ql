-- Array values are bounded by their durable 16-bit element count and statement
-- memory, not by the former 1,024-element execution scratch. This file runs
-- unchanged on PostgreSQL 18.

CREATE TABLE array_value_width(
  id integer PRIMARY KEY,
  integers integer[],
  words text[]
);

INSERT INTO array_value_width
SELECT 1,
       array_agg(value ORDER BY value),
       array_agg('v' || value::text ORDER BY value)
FROM generate_series(1, 1100) AS values(value);

SELECT cardinality(integers), integers[1], integers[1100],
       cardinality(words), words[1], words[1100]
FROM array_value_width;

SELECT array_length(('{' || repeat('1,', 1099) || '1}')::integer[], 1);
SELECT array_length(string_to_array(repeat('x,', 1099) || 'x', ','), 1);
SELECT array_length(regexp_split_to_array(repeat('y,', 1099) || 'y', ','), 1);

SELECT cardinality(array_append(integers, 1101)),
       (array_append(integers, 1101))[1101],
       cardinality(array_prepend(0, integers)),
       (array_prepend(0, integers))[1],
       cardinality(array_cat(integers, integers)),
       cardinality(array_remove(integers, 550)),
       (array_replace(integers, 550, -550))[550],
       cardinality(trim_array(integers, 100))
FROM array_value_width;

SELECT array_position(integers, 1100),
       length(array_to_string(
         ('{' || repeat('z,', 1099) || 'z}')::text[], ',')),
       length(concat(VARIADIC
         ('{' || repeat('z,', 1099) || 'z}')::text[]))
FROM array_value_width;

SELECT length(format(
  repeat('%s', 1100),
  VARIADIC string_to_array(repeat('zzzz,', 1099) || 'zzzz', ',')
));

SELECT integers = integers,
       integers < array_append(integers, 1101),
       1100 = ANY(integers),
       integers @> ARRAY[1100]
FROM array_value_width;

SELECT length(array_to_json(
         ('{' || repeat('1,', 1099) || '1}')::integer[])::text),
       length((('{' || repeat('1,', 1099) || '1}')::integer[])::text);

-- simulator-backed external run
SELECT count(*), min(value), max(value)
FROM array_value_width, LATERAL unnest(integers) AS values(value);

-- cleanup
DROP TABLE array_value_width;
