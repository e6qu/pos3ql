-- An OR whose arms read two or more keys of a statistics object with an MCV
-- list is estimated through it (statext_mcv_clauselist_selectivity with
-- is_or), and MCV lists of more column types are read.
CREATE TABLE es836 (a int, b int, c int, t time, n name, i interval, x bytea);
INSERT INTO es836
  SELECT g % 10, (g % 10) / 2, g % 7,
         make_time(g % 5, 0, 0), ('n' || (g % 5))::name,
         make_interval(days => g % 4), decode(lpad(to_hex(g % 6), 2, '0'), 'hex')
  FROM generate_series(1, 3000) g;
CREATE STATISTICS es836_ab (mcv) ON a, b FROM es836;
CREATE STATISTICS es836_tn (mcv) ON t, n FROM es836;
CREATE STATISTICS es836_ix (mcv) ON i, x FROM es836;
ANALYZE es836;
EXPLAIN SELECT * FROM es836 WHERE a = 1 OR b = 0;
EXPLAIN SELECT * FROM es836 WHERE a = 1 OR b = 3;
EXPLAIN SELECT * FROM es836 WHERE (a = 1 AND b = 0) OR (a = 4 AND b = 2);
EXPLAIN SELECT * FROM es836 WHERE a = 1 OR b = 0 OR c = 3;
EXPLAIN SELECT * FROM es836 WHERE a < 3 OR b > 3;
EXPLAIN SELECT * FROM es836 WHERE (a = 1 OR b = 0) AND c = 2;
EXPLAIN SELECT * FROM es836 WHERE a IN (1, 2) OR b IS NULL;
EXPLAIN SELECT * FROM es836 WHERE t = '01:00' AND n = 'n1';
EXPLAIN SELECT * FROM es836 WHERE t = '01:00' OR n = 'n2';
EXPLAIN SELECT * FROM es836 WHERE i = '1 day' AND x = '\x01';
EXPLAIN SELECT * FROM es836 WHERE i < '2 days' OR x = '\x03';
