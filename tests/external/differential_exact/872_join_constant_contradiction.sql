-- An equivalence class equating two different constants in the query's own
-- join domain: the constants' equality is false
-- (generate_base_implied_equalities_const), so the joined relation is dummy
-- and the query runs a Result whose One-Time Filter is false, under any sort,
-- limit or aggregate.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/equivclass.c
CREATE TABLE ca872 (id int, k int, v int);
CREATE TABLE cb872 (id int, k int, v int);
CREATE TABLE cc872 (id int, k int, v int);
INSERT INTO ca872 SELECT g, g % 50, g % 13 FROM generate_series(1, 800) g;
INSERT INTO cb872 SELECT g, g % 300, g % 13 FROM generate_series(1, 600) g;
INSERT INTO cc872 SELECT g, g % 20, g % 13 FROM generate_series(1, 100) g;
CREATE INDEX cb872_k ON cb872 (k);
ANALYZE ca872; ANALYZE cb872; ANALYZE cc872;
EXPLAIN SELECT ca872.id, cb872.id FROM ca872 JOIN cb872 ON ca872.k = cb872.k WHERE ca872.k = 1 AND cb872.k = 2;
SELECT ca872.id, cb872.id FROM ca872 JOIN cb872 ON ca872.k = cb872.k WHERE ca872.k = 1 AND cb872.k = 2;
EXPLAIN SELECT ca872.id, cb872.id FROM ca872 JOIN cb872 ON ca872.k = cb872.k WHERE ca872.k = 1 AND ca872.k = 2;
EXPLAIN SELECT ca872.id, cb872.id FROM ca872 LEFT JOIN cb872 ON ca872.k = cb872.k WHERE cb872.k = 1 AND cb872.k = 2;
EXPLAIN SELECT ca872.id, cb872.id, cc872.id FROM ca872 JOIN cb872 ON ca872.k = cb872.k LEFT JOIN cc872 ON cc872.v = ca872.v WHERE ca872.k = 1 AND cb872.k = 2;
SELECT ca872.id, cb872.id, cc872.id FROM ca872 JOIN cb872 ON ca872.k = cb872.k LEFT JOIN cc872 ON cc872.v = ca872.v WHERE ca872.k = 1 AND cb872.k = 2;
EXPLAIN SELECT ca872.id, cb872.id FROM ca872 JOIN cb872 ON ca872.k = cb872.k WHERE ca872.k = 1 AND cb872.k = 2 ORDER BY ca872.id LIMIT 3;
SELECT ca872.id, cb872.id FROM ca872 JOIN cb872 ON ca872.k = cb872.k WHERE ca872.k = 1 AND cb872.k = 2 ORDER BY ca872.id LIMIT 3;
EXPLAIN SELECT count(*) FROM ca872 JOIN cb872 ON ca872.k = cb872.k WHERE ca872.k = 1 AND cb872.k = 2;
SELECT count(*) FROM ca872 JOIN cb872 ON ca872.k = cb872.k WHERE ca872.k = 1 AND cb872.k = 2;
