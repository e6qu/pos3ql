-- generate_series over numeric and timestamps estimates its rows as its
-- support function does (generate_series_numeric_support,
-- generate_series_timestamp_support): from constant bounds and step, none
-- for a NULL argument, the function's 1000 rows for NaN or infinite values.
EXPLAIN SELECT * FROM generate_series(1.0, 2.0, 0.1) g;
EXPLAIN SELECT * FROM generate_series(1.5, 10) g;
EXPLAIN SELECT * FROM generate_series(10.0, 1, 0.5) g;
EXPLAIN SELECT * FROM generate_series(10.0, 1, -0.3) g;
EXPLAIN SELECT * FROM generate_series(1, 1, -1.0) g;
EXPLAIN SELECT * FROM generate_series(1.0, 'NaN', 1) g;
EXPLAIN SELECT * FROM generate_series(1.0, NULL, 1) g;
EXPLAIN SELECT * FROM generate_series(1.0, 5, 0) g;
EXPLAIN SELECT * FROM generate_series('2024-01-01'::timestamp, '2024-01-02', '1 hour') g;
EXPLAIN SELECT * FROM generate_series('2024-01-01'::timestamptz, '2024-03-01', '1 month') g;
EXPLAIN SELECT * FROM generate_series('2024-01-02'::timestamp, '2024-01-01', '-7 hours') g;
EXPLAIN SELECT * FROM generate_series('2024-01-01'::timestamp, 'infinity', '1 day') g;
EXPLAIN SELECT * FROM generate_series('2024-01-01'::timestamptz, '2024-01-03', '12 hours', 'UTC') g;
EXPLAIN SELECT * FROM generate_series(1, 100) g WHERE g > 50;
EXPLAIN SELECT * FROM generate_series(1, 10.5, 2) g;
SELECT count(*) FROM generate_series(1.0, 2.0, 0.1) g;
