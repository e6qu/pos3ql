-- EXPLAIN ANALYZE runs the plan GEQO chose and counts each node's rows.
CREATE TABLE ga1 (id int, k int);
CREATE TABLE ga2 (id int, k int);
CREATE TABLE ga3 (id int, k int);
CREATE TABLE ga4 (id int, k int);
CREATE TABLE ga5 (id int, k int);
CREATE TABLE ga6 (id int, k int);
CREATE TABLE ga7 (id int, k int);
CREATE TABLE ga8 (id int, k int);
CREATE TABLE ga9 (id int, k int);
CREATE TABLE ga10 (id int, k int);
CREATE TABLE ga11 (id int, k int);
CREATE TABLE ga12 (id int, k int);
INSERT INTO ga1 SELECT g, g % 7 FROM generate_series(1, 120) g;
INSERT INTO ga2 SELECT g, g % 5 FROM generate_series(1, 80) g;
INSERT INTO ga3 SELECT g, g % 3 FROM generate_series(1, 60) g;
INSERT INTO ga4 SELECT g, g % 11 FROM generate_series(1, 150) g;
INSERT INTO ga5 SELECT g, g % 2 FROM generate_series(1, 40) g;
INSERT INTO ga6 SELECT g, g % 9 FROM generate_series(1, 90) g;
INSERT INTO ga7 SELECT g, g % 4 FROM generate_series(1, 30) g;
INSERT INTO ga8 SELECT g, g % 6 FROM generate_series(1, 70) g;
INSERT INTO ga9 SELECT g, g % 8 FROM generate_series(1, 110) g;
INSERT INTO ga10 SELECT g, g % 10 FROM generate_series(1, 50) g;
INSERT INTO ga11 SELECT g, g % 12 FROM generate_series(1, 130) g;
INSERT INTO ga12 SELECT g, g % 13 FROM generate_series(1, 20) g;
ANALYZE ga1, ga2, ga3, ga4, ga5, ga6, ga7, ga8, ga9, ga10, ga11, ga12;
EXPLAIN (ANALYZE, BUFFERS OFF, COSTS OFF, TIMING OFF, SUMMARY OFF)
SELECT count(*) FROM ga1, ga2, ga3, ga4, ga5, ga6, ga7, ga8, ga9, ga10, ga11, ga12
  WHERE ga1.id = ga2.id AND ga2.id = ga3.id AND ga3.k = ga4.k AND ga4.id = ga5.id
    AND ga5.k = ga6.k AND ga6.id = ga7.id AND ga7.id = ga8.id AND ga8.k = ga9.k
    AND ga9.id = ga10.id AND ga10.id = ga11.id AND ga11.k = ga12.k;
EXPLAIN (ANALYZE, BUFFERS OFF, TIMING OFF, SUMMARY OFF)
SELECT count(*) FROM ga1, ga2, ga3, ga4, ga5, ga6, ga7, ga8, ga9, ga10, ga11, ga12
  WHERE ga1.id = ga2.id AND ga2.id = ga3.id AND ga3.k = ga4.k AND ga4.id = ga5.id
    AND ga5.k = ga6.k AND ga6.id = ga7.id AND ga7.id = ga8.id AND ga8.k = ga9.k
    AND ga9.id = ga10.id AND ga10.id = ga11.id AND ga11.k = ga12.k;
