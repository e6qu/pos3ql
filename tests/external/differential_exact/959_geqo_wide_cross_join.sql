-- A join of twenty relations, only some of them joined by conditions:
-- GEQO joins the related ones first and the rest by force (cross joins).
CREATE TABLE gw1 (id int);
CREATE TABLE gw2 (id int);
CREATE TABLE gw3 (id int);
CREATE TABLE gw4 (id int);
CREATE TABLE gw5 (id int);
CREATE TABLE gw6 (id int);
CREATE TABLE gw7 (id int);
CREATE TABLE gw8 (id int);
CREATE TABLE gw9 (id int);
CREATE TABLE gw10 (id int);
CREATE TABLE gw11 (id int);
CREATE TABLE gw12 (id int);
CREATE TABLE gw13 (id int);
CREATE TABLE gw14 (id int);
CREATE TABLE gw15 (id int);
CREATE TABLE gw16 (id int);
CREATE TABLE gw17 (id int);
CREATE TABLE gw18 (id int);
CREATE TABLE gw19 (id int);
CREATE TABLE gw20 (id int);
INSERT INTO gw1 SELECT g FROM generate_series(1, 2) g;
INSERT INTO gw2 SELECT g FROM generate_series(1, 3) g;
INSERT INTO gw3 SELECT g FROM generate_series(1, 4) g;
INSERT INTO gw4 SELECT g FROM generate_series(1, 1) g;
INSERT INTO gw5 SELECT g FROM generate_series(1, 2) g;
INSERT INTO gw6 SELECT g FROM generate_series(1, 3) g;
INSERT INTO gw7 SELECT g FROM generate_series(1, 4) g;
INSERT INTO gw8 SELECT g FROM generate_series(1, 1) g;
INSERT INTO gw9 SELECT g FROM generate_series(1, 2) g;
INSERT INTO gw10 SELECT g FROM generate_series(1, 3) g;
INSERT INTO gw11 SELECT g FROM generate_series(1, 4) g;
INSERT INTO gw12 SELECT g FROM generate_series(1, 1) g;
INSERT INTO gw13 SELECT g FROM generate_series(1, 2) g;
INSERT INTO gw14 SELECT g FROM generate_series(1, 3) g;
INSERT INTO gw15 SELECT g FROM generate_series(1, 4) g;
INSERT INTO gw16 SELECT g FROM generate_series(1, 1) g;
INSERT INTO gw17 SELECT g FROM generate_series(1, 2) g;
INSERT INTO gw18 SELECT g FROM generate_series(1, 3) g;
INSERT INTO gw19 SELECT g FROM generate_series(1, 4) g;
INSERT INTO gw20 SELECT g FROM generate_series(1, 1) g;
ANALYZE gw1, gw2, gw3, gw4, gw5, gw6, gw7, gw8, gw9, gw10, gw11, gw12, gw13, gw14, gw15, gw16, gw17, gw18, gw19, gw20;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gw1, gw2, gw3, gw4, gw5, gw6, gw7, gw8, gw9, gw10, gw11, gw12, gw13, gw14, gw15, gw16, gw17, gw18, gw19, gw20
  WHERE gw1.id = gw2.id AND gw3.id = gw4.id AND gw4.id = gw5.id AND gw10.id = gw11.id;
SELECT count(*) FROM gw1, gw2, gw3, gw4, gw5, gw6, gw7, gw8, gw9, gw10, gw11, gw12, gw13, gw14, gw15, gw16, gw17, gw18, gw19, gw20
  WHERE gw1.id = gw2.id AND gw3.id = gw4.id AND gw4.id = gw5.id AND gw10.id = gw11.id;
EXPLAIN (COSTS OFF)
SELECT gw1.id + gw20.id FROM gw1, gw2, gw3, gw4, gw5, gw6, gw7, gw8, gw9, gw10, gw11, gw12, gw13, gw14, gw15, gw16, gw17, gw18, gw19, gw20 LIMIT 1;
SELECT gw1.id + gw20.id FROM gw1, gw2, gw3, gw4, gw5, gw6, gw7, gw8, gw9, gw10, gw11, gw12, gw13, gw14, gw15, gw16, gw17, gw18, gw19, gw20 LIMIT 1;
