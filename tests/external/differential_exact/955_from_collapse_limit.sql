-- from_collapse_limit keeps a FROM list whose items would exceed it as
-- separate join problems, each planned on its own (deconstruct_jointree).
CREATE TABLE fc1 (id int, k int);
CREATE TABLE fc2 (id int, k int);
CREATE TABLE fc3 (id int, k int);
CREATE TABLE fc4 (id int, k int);
CREATE TABLE fc5 (id int, k int);
CREATE TABLE fc6 (id int, k int);
CREATE TABLE fc7 (id int, k int);
CREATE TABLE fc8 (id int, k int);
INSERT INTO fc1 SELECT g, g % 7 FROM generate_series(1, 120) g;
INSERT INTO fc2 SELECT g, g % 5 FROM generate_series(1, 80) g;
INSERT INTO fc3 SELECT g, g % 3 FROM generate_series(1, 60) g;
INSERT INTO fc4 SELECT g, g % 11 FROM generate_series(1, 150) g;
INSERT INTO fc5 SELECT g, g % 2 FROM generate_series(1, 40) g;
INSERT INTO fc6 SELECT g, g % 9 FROM generate_series(1, 90) g;
INSERT INTO fc7 SELECT g, g % 4 FROM generate_series(1, 30) g;
INSERT INTO fc8 SELECT g, g % 6 FROM generate_series(1, 70) g;
ANALYZE fc1, fc2, fc3, fc4, fc5, fc6, fc7, fc8;
SET from_collapse_limit = 3;
EXPLAIN
SELECT count(*) FROM fc1, fc2 JOIN fc3 ON fc2.id = fc3.id, fc4 JOIN fc5 ON fc4.k = fc5.k
  WHERE fc1.id = fc2.id AND fc3.k = fc4.k;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM fc1, (fc2 JOIN fc3 ON fc2.id = fc3.id) JOIN fc6 ON fc6.id = fc3.id, fc4
  WHERE fc1.id = fc2.id AND fc4.id = fc6.id;
SELECT count(*) FROM fc1, fc2 JOIN fc3 ON fc2.id = fc3.id, fc4 JOIN fc5 ON fc4.k = fc5.k
  WHERE fc1.id = fc2.id AND fc3.k = fc4.k;
SET from_collapse_limit = 1;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM fc1, fc2 JOIN fc3 ON fc2.id = fc3.id, fc4
  WHERE fc1.id = fc2.id AND fc3.k = fc4.k;
RESET from_collapse_limit;
-- Below the limit the same FROM list is one problem.
EXPLAIN
SELECT count(*) FROM fc1, fc2 JOIN fc3 ON fc2.id = fc3.id, fc4 JOIN fc5 ON fc4.k = fc5.k
  WHERE fc1.id = fc2.id AND fc3.k = fc4.k;
SET from_collapse_limit = 0;
