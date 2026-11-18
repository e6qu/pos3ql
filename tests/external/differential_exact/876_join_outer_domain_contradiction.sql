-- A class equating two different constants inside an outer join's nullable
-- side: the constants' false equality applies to the domain's relations
-- apart from lower left joins' nullable sides (get_join_domain_min_rels),
-- one relation becoming dummy (relation_excluded_by_constraints) and
-- several making their join dummy (populate_joinrel_with_paths), so only
-- the inner side is empty: a Result whose One-Time Filter is false, with
-- its never-scanned relation's columns printed unqualified and sized zero.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/initsplan.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/joinrels.c
CREATE TABLE ca876 (id int, k int, v int);
CREATE TABLE cb876 (id int, k int, v int);
CREATE TABLE cc876 (id int, k int, v int);
INSERT INTO ca876 SELECT g, g % 10, g % 7 FROM generate_series(1, 200) g;
INSERT INTO cb876 SELECT g, g % 12, g % 5 FROM generate_series(1, 150) g;
INSERT INTO cc876 SELECT g, g % 9, g % 4 FROM generate_series(1, 100) g;
ANALYZE ca876; ANALYZE cb876; ANALYZE cc876;
EXPLAIN SELECT ca876.id, cb876.id FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k AND cb876.v = 1 AND cb876.v = 2;
SELECT ca876.id, cb876.id FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k AND cb876.v = 1 AND cb876.v = 2 ORDER BY ca876.id LIMIT 5;
EXPLAIN SELECT ca876.id, cb876.id, cc876.id FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k LEFT JOIN cc876 ON cb876.k = cc876.k AND cc876.v = 1 AND cc876.v = 3;
SELECT ca876.id, cb876.id, cc876.id FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k LEFT JOIN cc876 ON cb876.k = cc876.k AND cc876.v = 1 AND cc876.v = 3 ORDER BY ca876.id, cb876.id LIMIT 5;
EXPLAIN SELECT ca876.id FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k AND cb876.v = 1 AND cb876.v = 2 WHERE cb876.id IS NULL;
SELECT count(*) FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k AND cb876.v = 1 AND cb876.v = 2 WHERE cb876.id IS NULL;
EXPLAIN SELECT ca876.id, cb876.id FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k AND cb876.v = 1 AND cb876.v = 2 ORDER BY cb876.k, ca876.id LIMIT 4;
EXPLAIN SELECT ca876.id, cb876.id, cc876.id FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k AND cb876.v = 1 AND cb876.v = 2 JOIN cc876 ON cc876.k = ca876.k;
SELECT count(*), count(cb876.id) FROM ca876 LEFT JOIN cb876 ON ca876.k = cb876.k AND cb876.v = 1 AND cb876.v = 2 JOIN cc876 ON cc876.k = ca876.k;
