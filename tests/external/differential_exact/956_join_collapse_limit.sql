-- join_collapse_limit stops flattening explicit JOINs into one join
-- problem; at 1 the written join order is kept.
CREATE TABLE jc1 (id int, k int);
CREATE TABLE jc2 (id int, k int);
CREATE TABLE jc3 (id int, k int);
CREATE TABLE jc4 (id int, k int);
CREATE TABLE jc5 (id int, k int);
CREATE TABLE jc6 (id int, k int);
CREATE TABLE jc7 (id int, k int);
CREATE TABLE jc8 (id int, k int);
INSERT INTO jc1 SELECT g, g % 7 FROM generate_series(1, 120) g;
INSERT INTO jc2 SELECT g, g % 5 FROM generate_series(1, 80) g;
INSERT INTO jc3 SELECT g, g % 3 FROM generate_series(1, 60) g;
INSERT INTO jc4 SELECT g, g % 11 FROM generate_series(1, 150) g;
INSERT INTO jc5 SELECT g, g % 2 FROM generate_series(1, 40) g;
INSERT INTO jc6 SELECT g, g % 9 FROM generate_series(1, 90) g;
INSERT INTO jc7 SELECT g, g % 4 FROM generate_series(1, 30) g;
INSERT INTO jc8 SELECT g, g % 6 FROM generate_series(1, 70) g;
ANALYZE jc1, jc2, jc3, jc4, jc5, jc6, jc7, jc8;
SET join_collapse_limit = 1;
EXPLAIN
SELECT count(*) FROM jc1 JOIN jc2 ON jc1.id = jc2.id JOIN jc3 ON jc2.k = jc3.k
  JOIN jc4 ON jc3.id = jc4.id JOIN jc5 ON jc4.id = jc5.id;
SET join_collapse_limit = 3;
EXPLAIN
SELECT count(*) FROM jc1 JOIN jc2 ON jc1.id = jc2.id JOIN jc3 ON jc2.k = jc3.k
  JOIN jc4 ON jc3.id = jc4.id JOIN jc5 ON jc4.id = jc5.id JOIN jc6 ON jc5.k = jc6.k;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM jc1 JOIN (jc2 JOIN jc3 ON jc2.k = jc3.k) ON jc1.id = jc2.id
  JOIN (jc4 JOIN jc5 ON jc4.id = jc5.id) ON jc3.id = jc4.id;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM jc1 LEFT JOIN jc2 ON jc1.id = jc2.id JOIN jc3 ON jc1.k = jc3.k
  LEFT JOIN jc4 ON jc3.id = jc4.id JOIN jc5 ON jc1.id = jc5.id;
SELECT count(*) FROM jc1 JOIN jc2 ON jc1.id = jc2.id JOIN jc3 ON jc2.k = jc3.k
  JOIN jc4 ON jc3.id = jc4.id JOIN jc5 ON jc4.id = jc5.id JOIN jc6 ON jc5.k = jc6.k;
RESET join_collapse_limit;
SET join_collapse_limit = 0;
