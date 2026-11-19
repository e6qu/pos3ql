-- geqo_seed picks the genetic search's random numbers (PostgreSQL's
-- pg_prng seeded from it), so each seed gives its own fixed plan.
CREATE TABLE gs1 (id int, k int);
CREATE TABLE gs2 (id int, k int);
CREATE TABLE gs3 (id int, k int);
CREATE TABLE gs4 (id int, k int);
CREATE TABLE gs5 (id int, k int);
CREATE TABLE gs6 (id int, k int);
CREATE TABLE gs7 (id int, k int);
CREATE TABLE gs8 (id int, k int);
CREATE TABLE gs9 (id int, k int);
CREATE TABLE gs10 (id int, k int);
CREATE TABLE gs11 (id int, k int);
CREATE TABLE gs12 (id int, k int);
INSERT INTO gs1 SELECT g, g % 7 FROM generate_series(1, 120) g;
INSERT INTO gs2 SELECT g, g % 5 FROM generate_series(1, 80) g;
INSERT INTO gs3 SELECT g, g % 3 FROM generate_series(1, 60) g;
INSERT INTO gs4 SELECT g, g % 11 FROM generate_series(1, 150) g;
INSERT INTO gs5 SELECT g, g % 2 FROM generate_series(1, 40) g;
INSERT INTO gs6 SELECT g, g % 9 FROM generate_series(1, 90) g;
INSERT INTO gs7 SELECT g, g % 4 FROM generate_series(1, 30) g;
INSERT INTO gs8 SELECT g, g % 6 FROM generate_series(1, 70) g;
INSERT INTO gs9 SELECT g, g % 8 FROM generate_series(1, 110) g;
INSERT INTO gs10 SELECT g, g % 10 FROM generate_series(1, 50) g;
INSERT INTO gs11 SELECT g, g % 12 FROM generate_series(1, 130) g;
INSERT INTO gs12 SELECT g, g % 13 FROM generate_series(1, 20) g;
ANALYZE gs1, gs2, gs3, gs4, gs5, gs6, gs7, gs8, gs9, gs10, gs11, gs12;
SET geqo_seed = 0.5;
SHOW geqo_seed;
EXPLAIN
SELECT count(*) FROM gs1, gs2, gs3, gs4, gs5, gs6, gs7, gs8, gs9, gs10, gs11, gs12
  WHERE gs1.id = gs2.id AND gs2.id = gs3.id AND gs3.k = gs4.k AND gs4.id = gs5.id
    AND gs5.k = gs6.k AND gs6.id = gs7.id AND gs7.id = gs8.id AND gs8.k = gs9.k
    AND gs9.id = gs10.id AND gs10.id = gs11.id AND gs11.k = gs12.k;
SET geqo_seed = 1;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM gs1, gs2, gs3, gs4, gs5, gs6, gs7, gs8, gs9, gs10, gs11, gs12
  WHERE gs1.id = gs2.id AND gs2.id = gs3.id AND gs3.k = gs4.k AND gs4.id = gs5.id
    AND gs5.k = gs6.k AND gs6.id = gs7.id AND gs7.id = gs8.id AND gs8.k = gs9.k
    AND gs9.id = gs10.id AND gs10.id = gs11.id AND gs11.k = gs12.k;
SET geqo_seed = 0.123456;
EXPLAIN
SELECT count(*) FROM gs1, gs2, gs3, gs4, gs5, gs6, gs7, gs8, gs9, gs10, gs11, gs12
  WHERE gs1.id = gs2.id AND gs2.id = gs3.id AND gs3.k = gs4.k AND gs4.id = gs5.id
    AND gs5.k = gs6.k AND gs6.id = gs7.id AND gs7.id = gs8.id AND gs8.k = gs9.k
    AND gs9.id = gs10.id AND gs10.id = gs11.id AND gs11.k = gs12.k;
SELECT count(*) FROM gs1, gs2, gs3, gs4, gs5, gs6, gs7, gs8, gs9, gs10, gs11, gs12
  WHERE gs1.id = gs2.id AND gs2.id = gs3.id AND gs3.k = gs4.k AND gs4.id = gs5.id
    AND gs5.k = gs6.k AND gs6.id = gs7.id AND gs7.id = gs8.id AND gs8.k = gs9.k
    AND gs9.id = gs10.id AND gs10.id = gs11.id AND gs11.k = gs12.k;
SET geqo_seed = 1.5;
SET geqo_seed = -0.1;
RESET geqo_seed;
SHOW geqo_seed;
