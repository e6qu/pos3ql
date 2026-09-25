-- Multirange component breadth follows value bytes and statement memory.
CREATE TABLE multirange_value_width (
  id integer PRIMARY KEY,
  spans int4multirange NOT NULL
);

INSERT INTO multirange_value_width
SELECT 1, range_agg(int4range(value * 3, value * 3 + 1))
FROM generate_series(0, 127) value;

CREATE INDEX multirange_value_width_spans
ON multirange_value_width USING gist (spans);

SELECT count(*), min(lower(value)), max(upper(value))
FROM multirange_value_width, unnest(spans) AS expanded(value);

SELECT count(*)
FROM multirange_value_width,
     unnest(spans + '{[400,401)}') AS expanded(value);

SELECT count(*)
FROM multirange_value_width,
     unnest(spans - '{[3,4)}') AS expanded(value);

SELECT count(*)
FROM multirange_value_width,
     unnest(spans * '{[0,382)}') AS expanded(value);

SELECT range_merge(spans), lower_inc(spans), upper_inf(spans),
       hash_multirange(spans) = hash_multirange(spans)
FROM multirange_value_width;

SELECT id
FROM multirange_value_width
WHERE spans && '{[381,382)}';

SELECT lower(value), upper(value)
FROM multirange_value_width, unnest(spans) WITH ORDINALITY AS expanded(value, ordinal)
WHERE ordinal IN (1, 65, 128)
ORDER BY ordinal;

DROP TABLE multirange_value_width;
