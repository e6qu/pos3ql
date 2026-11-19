-- geqo_effort sizes the default pool (between 10 and 50 individuals per
-- unit of effort) and so the generations; geqo_pool_size, geqo_generations
-- and geqo_selection_bias set them directly.
CREATE TABLE ge1 (id int, k int);
CREATE TABLE ge2 (id int, k int);
CREATE TABLE ge3 (id int, k int);
CREATE TABLE ge4 (id int, k int);
CREATE TABLE ge5 (id int, k int);
CREATE TABLE ge6 (id int, k int);
CREATE TABLE ge7 (id int, k int);
CREATE TABLE ge8 (id int, k int);
CREATE TABLE ge9 (id int, k int);
CREATE TABLE ge10 (id int, k int);
CREATE TABLE ge11 (id int, k int);
CREATE TABLE ge12 (id int, k int);
INSERT INTO ge1 SELECT g, g % 7 FROM generate_series(1, 120) g;
INSERT INTO ge2 SELECT g, g % 5 FROM generate_series(1, 80) g;
INSERT INTO ge3 SELECT g, g % 3 FROM generate_series(1, 60) g;
INSERT INTO ge4 SELECT g, g % 11 FROM generate_series(1, 150) g;
INSERT INTO ge5 SELECT g, g % 2 FROM generate_series(1, 40) g;
INSERT INTO ge6 SELECT g, g % 9 FROM generate_series(1, 90) g;
INSERT INTO ge7 SELECT g, g % 4 FROM generate_series(1, 30) g;
INSERT INTO ge8 SELECT g, g % 6 FROM generate_series(1, 70) g;
INSERT INTO ge9 SELECT g, g % 8 FROM generate_series(1, 110) g;
INSERT INTO ge10 SELECT g, g % 10 FROM generate_series(1, 50) g;
INSERT INTO ge11 SELECT g, g % 12 FROM generate_series(1, 130) g;
INSERT INTO ge12 SELECT g, g % 13 FROM generate_series(1, 20) g;
ANALYZE ge1, ge2, ge3, ge4, ge5, ge6, ge7, ge8, ge9, ge10, ge11, ge12;
SET geqo_effort = 1;
EXPLAIN
SELECT count(*) FROM ge1, ge2, ge3, ge4, ge5, ge6, ge7, ge8, ge9, ge10, ge11, ge12
  WHERE ge1.id = ge2.id AND ge2.id = ge3.id AND ge3.k = ge4.k AND ge4.id = ge5.id
    AND ge5.k = ge6.k AND ge6.id = ge7.id AND ge7.id = ge8.id AND ge8.k = ge9.k
    AND ge9.id = ge10.id AND ge10.id = ge11.id AND ge11.k = ge12.k;
SET geqo_effort = 10;
EXPLAIN
SELECT count(*) FROM ge1, ge2, ge3, ge4, ge5, ge6, ge7, ge8, ge9, ge10, ge11, ge12
  WHERE ge1.id = ge2.id AND ge2.id = ge3.id AND ge3.k = ge4.k AND ge4.id = ge5.id
    AND ge5.k = ge6.k AND ge6.id = ge7.id AND ge7.id = ge8.id AND ge8.k = ge9.k
    AND ge9.id = ge10.id AND ge10.id = ge11.id AND ge11.k = ge12.k;
RESET geqo_effort;
SET geqo_pool_size = 30;
SET geqo_generations = 7;
EXPLAIN
SELECT count(*) FROM ge1, ge2, ge3, ge4, ge5, ge6, ge7, ge8, ge9, ge10, ge11, ge12
  WHERE ge1.id = ge2.id AND ge2.id = ge3.id AND ge3.k = ge4.k AND ge4.id = ge5.id
    AND ge5.k = ge6.k AND ge6.id = ge7.id AND ge7.id = ge8.id AND ge8.k = ge9.k
    AND ge9.id = ge10.id AND ge10.id = ge11.id AND ge11.k = ge12.k;
SET geqo_selection_bias = 1.5;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM ge1, ge2, ge3, ge4, ge5, ge6, ge7, ge8, ge9, ge10, ge11, ge12
  WHERE ge1.id = ge2.id AND ge2.id = ge3.id AND ge3.k = ge4.k AND ge4.id = ge5.id
    AND ge5.k = ge6.k AND ge6.id = ge7.id AND ge7.id = ge8.id AND ge8.k = ge9.k
    AND ge9.id = ge10.id AND ge10.id = ge11.id AND ge11.k = ge12.k;
-- A pool of one individual is too small: the default size is used.
SET geqo_pool_size = 1;
SET geqo_generations = 3;
EXPLAIN
SELECT count(*) FROM ge1, ge2, ge3, ge4, ge5, ge6, ge7, ge8, ge9, ge10, ge11, ge12
  WHERE ge1.id = ge2.id AND ge2.id = ge3.id AND ge3.k = ge4.k AND ge4.id = ge5.id
    AND ge5.k = ge6.k AND ge6.id = ge7.id AND ge7.id = ge8.id AND ge8.k = ge9.k
    AND ge9.id = ge10.id AND ge10.id = ge11.id AND ge11.k = ge12.k;
SET geqo_effort = 0;
SET geqo_effort = 11;
SET geqo_selection_bias = 2.5;
SET geqo_pool_size = -1;
SET geqo_generations = -1;
RESET geqo_pool_size;
RESET geqo_generations;
RESET geqo_selection_bias;
SELECT name, setting, unit, category, short_desc, extra_desc, context, vartype,
  source, min_val, max_val, boot_val, reset_val
  FROM pg_settings WHERE name LIKE 'geqo%' OR name LIKE '%collapse_limit' ORDER BY name;
