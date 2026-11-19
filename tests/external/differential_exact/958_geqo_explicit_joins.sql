-- GEQO also plans explicit inner and outer joins once they flatten into
-- one problem of geqo_threshold relations or more; outer joins restrict
-- the orders it may choose.
CREATE TABLE gj1 (id int, k int);
CREATE TABLE gj2 (id int, k int);
CREATE TABLE gj3 (id int, k int);
CREATE TABLE gj4 (id int, k int);
CREATE TABLE gj5 (id int, k int);
CREATE TABLE gj6 (id int, k int);
CREATE TABLE gj7 (id int, k int);
CREATE TABLE gj8 (id int, k int);
CREATE TABLE gj9 (id int, k int);
CREATE TABLE gj10 (id int, k int);
CREATE TABLE gj11 (id int, k int);
CREATE TABLE gj12 (id int, k int);
CREATE TABLE gj13 (id int, k int);
CREATE TABLE gj14 (id int, k int);
INSERT INTO gj1 SELECT g, g % 7 FROM generate_series(1, 120) g;
INSERT INTO gj2 SELECT g, g % 5 FROM generate_series(1, 80) g;
INSERT INTO gj3 SELECT g, g % 3 FROM generate_series(1, 60) g;
INSERT INTO gj4 SELECT g, g % 11 FROM generate_series(1, 150) g;
INSERT INTO gj5 SELECT g, g % 2 FROM generate_series(1, 40) g;
INSERT INTO gj6 SELECT g, g % 9 FROM generate_series(1, 90) g;
INSERT INTO gj7 SELECT g, g % 4 FROM generate_series(1, 30) g;
INSERT INTO gj8 SELECT g, g % 6 FROM generate_series(1, 70) g;
INSERT INTO gj9 SELECT g, g % 8 FROM generate_series(1, 110) g;
INSERT INTO gj10 SELECT g, g % 10 FROM generate_series(1, 50) g;
INSERT INTO gj11 SELECT g, g % 12 FROM generate_series(1, 130) g;
INSERT INTO gj12 SELECT g, g % 13 FROM generate_series(1, 20) g;
INSERT INTO gj13 SELECT g, g % 14 FROM generate_series(1, 100) g;
INSERT INTO gj14 SELECT g, g % 5 FROM generate_series(1, 45) g;
ANALYZE gj1, gj2, gj3, gj4, gj5, gj6, gj7, gj8, gj9, gj10, gj11, gj12, gj13, gj14;
SET join_collapse_limit = 20;
EXPLAIN
SELECT count(*) FROM gj1 JOIN gj2 ON gj1.id = gj2.id JOIN gj3 ON gj2.id = gj3.id
  JOIN gj4 ON gj3.k = gj4.k JOIN gj5 ON gj4.id = gj5.id JOIN gj6 ON gj5.k = gj6.k
  JOIN gj7 ON gj6.id = gj7.id JOIN gj8 ON gj7.id = gj8.id JOIN gj9 ON gj8.k = gj9.k
  JOIN gj10 ON gj9.id = gj10.id JOIN gj11 ON gj10.id = gj11.id JOIN gj12 ON gj11.k = gj12.k;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gj1 LEFT JOIN gj2 ON gj1.id = gj2.id JOIN gj3 ON gj1.id = gj3.id
  JOIN gj4 ON gj3.k = gj4.k LEFT JOIN gj5 ON gj4.id = gj5.id JOIN gj6 ON gj4.k = gj6.k
  JOIN gj7 ON gj6.id = gj7.id JOIN gj8 ON gj7.id = gj8.id JOIN gj9 ON gj8.k = gj9.k
  LEFT JOIN gj10 ON gj9.id = gj10.id JOIN gj11 ON gj9.id = gj11.id JOIN gj12 ON gj11.k = gj12.k
  JOIN gj13 ON gj12.id = gj13.id;
SELECT count(*) FROM gj1 LEFT JOIN gj2 ON gj1.id = gj2.id JOIN gj3 ON gj1.id = gj3.id
  JOIN gj4 ON gj3.k = gj4.k LEFT JOIN gj5 ON gj4.id = gj5.id JOIN gj6 ON gj4.k = gj6.k
  JOIN gj7 ON gj6.id = gj7.id JOIN gj8 ON gj7.id = gj8.id JOIN gj9 ON gj8.k = gj9.k
  LEFT JOIN gj10 ON gj9.id = gj10.id JOIN gj11 ON gj9.id = gj11.id JOIN gj12 ON gj11.k = gj12.k
  JOIN gj13 ON gj12.id = gj13.id;
SET geqo = off;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gj1 JOIN gj2 ON gj1.id = gj2.id JOIN gj3 ON gj2.id = gj3.id
  JOIN gj4 ON gj3.k = gj4.k JOIN gj5 ON gj4.id = gj5.id JOIN gj6 ON gj5.k = gj6.k
  JOIN gj7 ON gj6.id = gj7.id JOIN gj8 ON gj7.id = gj8.id JOIN gj9 ON gj8.k = gj9.k;
RESET geqo;
RESET join_collapse_limit;
