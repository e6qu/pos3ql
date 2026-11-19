-- From geqo_threshold relations on PostgreSQL searches the join order
-- genetically (GEQO); at a fixed geqo_seed the search, and so the plan and
-- its costs, are the same every time.
CREATE TABLE gq1 (id int, k int);
CREATE TABLE gq2 (id int, k int);
CREATE TABLE gq3 (id int, k int);
CREATE TABLE gq4 (id int, k int);
CREATE TABLE gq5 (id int, k int);
CREATE TABLE gq6 (id int, k int);
CREATE TABLE gq7 (id int, k int);
CREATE TABLE gq8 (id int, k int);
CREATE TABLE gq9 (id int, k int);
CREATE TABLE gq10 (id int, k int);
CREATE TABLE gq11 (id int, k int);
CREATE TABLE gq12 (id int, k int);
CREATE TABLE gq13 (id int, k int);
INSERT INTO gq1 SELECT g, g % 7 FROM generate_series(1, 120) g;
INSERT INTO gq2 SELECT g, g % 5 FROM generate_series(1, 80) g;
INSERT INTO gq3 SELECT g, g % 3 FROM generate_series(1, 60) g;
INSERT INTO gq4 SELECT g, g % 11 FROM generate_series(1, 150) g;
INSERT INTO gq5 SELECT g, g % 2 FROM generate_series(1, 40) g;
INSERT INTO gq6 SELECT g, g % 9 FROM generate_series(1, 90) g;
INSERT INTO gq7 SELECT g, g % 4 FROM generate_series(1, 30) g;
INSERT INTO gq8 SELECT g, g % 6 FROM generate_series(1, 70) g;
INSERT INTO gq9 SELECT g, g % 8 FROM generate_series(1, 110) g;
INSERT INTO gq10 SELECT g, g % 10 FROM generate_series(1, 50) g;
INSERT INTO gq11 SELECT g, g % 12 FROM generate_series(1, 130) g;
INSERT INTO gq12 SELECT g, g % 13 FROM generate_series(1, 20) g;
INSERT INTO gq13 SELECT g, g % 14 FROM generate_series(1, 100) g;
ANALYZE gq1, gq2, gq3, gq4, gq5, gq6, gq7, gq8, gq9, gq10, gq11, gq12, gq13;
SHOW geqo;
SHOW geqo_threshold;
-- Twelve relations joined in a chain: GEQO plans them.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gq1, gq2, gq3, gq4, gq5, gq6, gq7, gq8, gq9, gq10, gq11, gq12
  WHERE gq1.id = gq2.id AND gq2.id = gq3.id AND gq3.k = gq4.k AND gq4.id = gq5.id
    AND gq5.k = gq6.k AND gq6.id = gq7.id AND gq7.id = gq8.id AND gq8.k = gq9.k
    AND gq9.id = gq10.id AND gq10.id = gq11.id AND gq11.k = gq12.k;
EXPLAIN
SELECT count(*) FROM gq1, gq2, gq3, gq4, gq5, gq6, gq7, gq8, gq9, gq10, gq11, gq12
  WHERE gq1.id = gq2.id AND gq2.id = gq3.id AND gq3.k = gq4.k AND gq4.id = gq5.id
    AND gq5.k = gq6.k AND gq6.id = gq7.id AND gq7.id = gq8.id AND gq8.k = gq9.k
    AND gq9.id = gq10.id AND gq10.id = gq11.id AND gq11.k = gq12.k;
SELECT count(*) FROM gq1, gq2, gq3, gq4, gq5, gq6, gq7, gq8, gq9, gq10, gq11, gq12
  WHERE gq1.id = gq2.id AND gq2.id = gq3.id AND gq3.k = gq4.k AND gq4.id = gq5.id
    AND gq5.k = gq6.k AND gq6.id = gq7.id AND gq7.id = gq8.id AND gq8.k = gq9.k
    AND gq9.id = gq10.id AND gq10.id = gq11.id AND gq11.k = gq12.k;
-- Thirteen relations, one joined to nothing: the clumps left over join
-- by force at the end.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gq1, gq2, gq3, gq4, gq5, gq6, gq7, gq8, gq9, gq10, gq11, gq12, gq13
  WHERE gq1.id = gq2.id AND gq2.id = gq3.id AND gq4.id = gq5.id AND gq5.id = gq6.id
    AND gq7.k = gq8.k AND gq9.id = gq10.id AND gq10.id = gq11.id AND gq12.id < 3
    AND gq13.id = gq1.id;
-- Below the threshold the exhaustive search plans the same query.
SET geqo_threshold = 14;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gq1, gq2, gq3, gq4, gq5, gq6, gq7, gq8, gq9, gq10, gq11, gq12
  WHERE gq1.id = gq2.id AND gq2.id = gq3.id AND gq3.k = gq4.k AND gq4.id = gq5.id
    AND gq5.k = gq6.k AND gq6.id = gq7.id AND gq7.id = gq8.id AND gq8.k = gq9.k
    AND gq9.id = gq10.id AND gq10.id = gq11.id AND gq11.k = gq12.k;
RESET geqo_threshold;
