-- A DISTINCT over the columns of one table that PostgreSQL plans as a
-- HashAggregate returns its rows in the hash table's order, like the
-- same grouping.
CREATE TABLE h924 (id int PRIMARY KEY, a int, b text, c numeric, d bigint, e date, f float8, t tsvector);
INSERT INTO h924
SELECT i, i % 97, 'k' || (i % 13), (i % 7) * 1.5, i % 1000, DATE '2020-01-01' + (i % 50), (i % 29) / 4.0,
       to_tsvector('simple', 'w' || (i % 5))
FROM generate_series(1, 3000) i;
ANALYZE h924;
SELECT DISTINCT a FROM h924;
SELECT DISTINCT b FROM h924;
SELECT DISTINCT a, b FROM h924;
SELECT DISTINCT b, a FROM h924;
SELECT DISTINCT d FROM h924;
SELECT DISTINCT c FROM h924;
SELECT DISTINCT e FROM h924;
SELECT DISTINCT f FROM h924;
SELECT DISTINCT a FROM h924 WHERE id > 1500;
SELECT DISTINCT d FROM h924 LIMIT 7;
SELECT DISTINCT a, a FROM h924;
SELECT DISTINCT t FROM h924;
EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM h924;
