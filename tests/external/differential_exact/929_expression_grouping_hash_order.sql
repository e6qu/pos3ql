-- A GROUP BY or DISTINCT of expressions is planned as PostgreSQL plans it:
-- estimate_num_groups reads an expression's columns (a boolean expression
-- makes two groups), the scan computes the keys, each group evaluates the
-- select list, and a HashAggregate returns its groups in its hash table's
-- order. A DISTINCT with an ORDER BY hashes, then sorts the groups, a
-- LIMIT bounding that sort. Arrays, records, ranges, multiranges and jsonb
-- hash as hash_array, hash_record, hash_range, hash_multirange and
-- jsonb_hash do; money, bit strings and anonymous records have no hash
-- operator class, so their groupings sort.
CREATE TABLE e929 (a int, b int, c text);
INSERT INTO e929 SELECT i % 100, i % 7, 'x' || (i % 13) FROM generate_series(1, 2000) i;
ANALYZE e929;
CREATE TABLE h929 (a int, b int, j jsonb, ar int[], r int4range,
  mr int4multirange, m money, bt bit(3));
INSERT INTO h929
SELECT i % 100, i % 7,
  jsonb_build_object('k', i % 9, 'v', array[i % 3, i % 2], 's', 'q"' || (i % 4)),
  array[i % 5, i % 3], int4range(i % 6, i % 6 + 3),
  int4multirange(int4range(i % 4, i % 4 + 1), int4range(10, 10 + i % 3)),
  (i % 4)::money, (i % 8)::bit(3)
FROM generate_series(1, 2000) i;
ANALYZE h929;
SELECT DISTINCT a + 1 FROM e929;
SELECT DISTINCT a % 10, b FROM e929;
SELECT a % 10, count(*) FROM e929 GROUP BY a % 10;
SELECT a + b, sum(a) FROM e929 GROUP BY a + b;
SELECT lower(c), count(*) FROM e929 GROUP BY lower(c);
SELECT a > 50, b, count(*) FROM e929 GROUP BY 1, 2;
SELECT (a % 10) + 1, count(*) + 1 FROM e929 GROUP BY a % 10;
SELECT DISTINCT a, b FROM e929 ORDER BY a;
SELECT DISTINCT a, b FROM e929 ORDER BY a LIMIT 10;
SELECT DISTINCT a % 7, b FROM e929 ORDER BY b DESC;
EXPLAIN SELECT a % 10, count(*) FROM e929 GROUP BY a % 10;
EXPLAIN SELECT a % 10 + b * 2, count(*) FROM e929 GROUP BY 1;
EXPLAIN SELECT a + b, sum(a) FROM e929 GROUP BY a + b ORDER BY 1;
EXPLAIN SELECT lower(c), count(*) FROM e929 GROUP BY lower(c);
SELECT DISTINCT j FROM h929;
SELECT DISTINCT ar FROM h929;
SELECT DISTINCT r FROM h929;
SELECT DISTINCT mr FROM h929;
SELECT DISTINCT m FROM h929;
SELECT DISTINCT bt FROM h929;
SELECT DISTINCT (a % 3, b) FROM h929;
SELECT j -> 'k', count(*) FROM h929 GROUP BY 1;
SELECT ar, r, count(*) FROM h929 GROUP BY ar, r;
DROP TABLE e929, h929;
