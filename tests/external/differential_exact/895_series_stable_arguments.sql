-- A support function estimates a series' rows from its stable arguments too
-- (`estimate_expression_value` evaluates `now()` and the like); an argument
-- reading a column or a volatile call gives the default estimate.
CREATE TABLE t895 (a integer, b integer);
INSERT INTO t895 SELECT g, g % 7 FROM generate_series(1, 30) g;
ANALYZE t895;
EXPLAIN SELECT * FROM generate_series(now(), now() + interval '1 day', interval '1 hour');
EXPLAIN SELECT * FROM generate_series(1, current_setting('server_version_num')::integer / 100000);
EXPLAIN SELECT * FROM generate_series(now()::date, now()::date + 9, interval '1 day');
EXPLAIN SELECT * FROM generate_series(1::bigint, extract(year FROM now())::bigint - 2020);
EXPLAIN SELECT * FROM generate_series(1, (random() * 0 + 5)::integer);
EXPLAIN SELECT * FROM unnest(string_to_array(to_char(now(), 'YYYY.MM.DD'), '.'));
EXPLAIN SELECT * FROM unnest(ARRAY[now()::text, 'x']);
EXPLAIN SELECT * FROM unnest(string_to_array(to_char(random(), '0'), '.'));
