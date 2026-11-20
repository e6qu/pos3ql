-- A HashAggregate over one table returns its groups in its simplehash
-- table's order: each group's hash is its key hashes combined by rotation
-- and murmurhash32, inserted in the order the groups' first rows arrive,
-- the table starting at the planner's group estimate and doubling as it
-- fills.
CREATE TABLE h922 (id int PRIMARY KEY, a int, b text, c numeric, d bigint, e date, f float8);
INSERT INTO h922
SELECT i, i % 97, 'k' || (i % 13), (i % 7) * 1.5, i % 1000, DATE '2020-01-01' + (i % 50), (i % 29) / 4.0
FROM generate_series(1, 3000) i;
ANALYZE h922;
SELECT a, count(*) FROM h922 GROUP BY a;
SELECT b FROM h922 GROUP BY b;
SELECT a, b, count(*) FROM h922 GROUP BY a, b;
SELECT b, a, count(*) FROM h922 GROUP BY a, b;
SELECT d, sum(a) FROM h922 GROUP BY d;
SELECT c, count(*) FROM h922 GROUP BY c;
SELECT e, count(*) FROM h922 GROUP BY e;
SELECT f, count(*) FROM h922 GROUP BY f;
SELECT b, count(*) FROM h922 GROUP BY b HAVING count(*) > 230;
SELECT a, count(*) FROM h922 WHERE id > 1500 GROUP BY a;
SELECT a, count(*) FROM h922 GROUP BY a ORDER BY count(*), a DESC;
SELECT d, count(*) FROM h922 GROUP BY d LIMIT 7;
