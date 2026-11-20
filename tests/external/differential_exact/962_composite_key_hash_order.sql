-- A named composite key hashes with hash_record over its fields' hash
-- functions, so a hashed grouping or DISTINCT of one returns its groups in
-- PostgreSQL's hash table order. A composite with a field type that has no
-- hash function (money) cannot hash, and its grouping sorts.
CREATE TYPE c962 AS (x int, y text);
CREATE TYPE m962 AS (x int, m money);
CREATE TABLE t962 (a int, b text, k c962, km m962, ka c962[]);
INSERT INTO t962 SELECT i % 40, 'b' || (i % 9), ROW(i % 30, 'y' || (i % 4))::c962,
  ROW(i % 6, (i % 5)::money)::m962, ARRAY[ROW(i % 20, 'z')::c962]
FROM generate_series(1, 1500) i;
ANALYZE t962;
SELECT DISTINCT k FROM t962;
SELECT k, count(*) FROM t962 GROUP BY k;
SELECT DISTINCT ROW(a, b)::c962 FROM t962 ORDER BY 1;
SELECT DISTINCT km FROM t962;
SELECT km, count(*) FROM t962 GROUP BY km;
SELECT DISTINCT k, a % 3 FROM t962;
SELECT DISTINCT ARRAY[k] FROM t962 ORDER BY 1;
SELECT DISTINCT ka FROM t962;
SELECT ka, count(*) FROM t962 GROUP BY ka;
SELECT DISTINCT ARRAY[ROW(a % 5, b)] FROM t962 ORDER BY 1;
DROP TABLE t962;
DROP TYPE c962, m962;
