-- The rows of each statement stay under the forced-spill run's 256 KiB memtable.
-- A condition on an expression a statistics object holds is estimated from
-- the expression's own statistics (`examine_variable` reads them when no
-- index does): equality through its most common values, ranges through its
-- histogram, a NULL test through its null fraction.
CREATE TABLE x900 (a integer, b text, c numeric, d timestamp);
INSERT INTO x900
SELECT g % 50, 'item' || (g % 17), (g % 23) * 1.5, timestamp '2024-01-01' + (g % 400) * interval '1 hour'
FROM generate_series(1, 1000) g;
INSERT INTO x900 SELECT NULL, NULL, NULL, NULL FROM generate_series(1, 100);
CREATE STATISTICS s900 (mcv) ON (a + 1), lower(b), (c * 2) FROM x900;
CREATE STATISTICS s900b ON (d::date) FROM x900;
ANALYZE x900;
EXPLAIN SELECT * FROM x900 WHERE a + 1 = 5;
EXPLAIN SELECT * FROM x900 WHERE a + 1 = 777;
EXPLAIN SELECT * FROM x900 WHERE a + 1 < 10;
EXPLAIN SELECT * FROM x900 WHERE a + 1 >= 10 AND a + 1 < 20;
EXPLAIN SELECT * FROM x900 WHERE lower(b) = 'item3';
EXPLAIN SELECT * FROM x900 WHERE lower(b) = 'nothing';
EXPLAIN SELECT * FROM x900 WHERE lower(b) > 'item5';
EXPLAIN SELECT * FROM x900 WHERE lower(b) IS NULL;
EXPLAIN SELECT * FROM x900 WHERE lower(b) IS NOT NULL;
EXPLAIN SELECT * FROM x900 WHERE c * 2 > 30;
EXPLAIN SELECT * FROM x900 WHERE c * 2 = 9;
EXPLAIN SELECT * FROM x900 WHERE d::date = '2024-01-05';
EXPLAIN SELECT * FROM x900 WHERE d::date BETWEEN '2024-01-05' AND '2024-01-09';
EXPLAIN SELECT * FROM x900 WHERE a + 1 IN (1, 2, 3);
EXPLAIN SELECT * FROM x900 WHERE a + 2 = 5;
EXPLAIN SELECT * FROM x900 WHERE a + 1 = 5 AND b = 'item5';
EXPLAIN SELECT * FROM x900 WHERE a + 1 = 5 AND d > '2024-01-10';
EXPLAIN SELECT * FROM x900 ORDER BY a + 1 LIMIT 5;
SELECT count(*) FROM x900 WHERE a + 1 = 5;
