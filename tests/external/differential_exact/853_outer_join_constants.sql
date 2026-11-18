-- A constant equated with a left or right join's outer column equates the
-- inner column with it too, inside the join's inner side, and the join
-- clause gives way to a constant true one that EXPLAIN does not print
-- (reconsider_outer_join_clause): both sides are filtered by the constant
-- and joined without a join clause.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/equivclass.c
CREATE TABLE ca853 (id int, k int, v int, w text);
CREATE TABLE cb853 (id int, k int, v int, w text);
CREATE TABLE cc853 (id int, k int, v int, w text);
INSERT INTO ca853 SELECT g, g % 50, g % 7, 'a' || g FROM generate_series(1, 400) g;
INSERT INTO cb853 SELECT g, g % 300, g % 11, 'b' || g FROM generate_series(1, 600) g;
INSERT INTO cc853 SELECT g, g % 40, g % 5, 'c' || g FROM generate_series(1, 300) g;
CREATE INDEX cb853_k ON cb853 (k);
ANALYZE ca853; ANALYZE cb853; ANALYZE cc853;
EXPLAIN SELECT ca853.id, cc853.id FROM ca853 LEFT JOIN cc853 ON ca853.k = cc853.k WHERE ca853.k = 5;
SELECT ca853.id, cc853.id FROM ca853 LEFT JOIN cc853 ON ca853.k = cc853.k WHERE ca853.k = 5 ORDER BY 1, 2 LIMIT 20;
EXPLAIN SELECT ca853.id, cb853.id FROM ca853 LEFT JOIN cb853 ON ca853.k = cb853.k WHERE ca853.k = 5;
SELECT ca853.id, cb853.id FROM ca853 LEFT JOIN cb853 ON ca853.k = cb853.k WHERE ca853.k = 5 ORDER BY 1, 2 LIMIT 20;
EXPLAIN SELECT ca853.id, cc853.id FROM cc853 RIGHT JOIN ca853 ON ca853.k = cc853.k AND ca853.v = cc853.v WHERE ca853.k = 7;
SELECT ca853.id, cc853.id FROM cc853 RIGHT JOIN ca853 ON ca853.k = cc853.k AND ca853.v = cc853.v WHERE ca853.k = 7 ORDER BY 1, 2;
EXPLAIN SELECT ca853.id, cb853.id, cc853.id FROM ca853 LEFT JOIN cb853 ON ca853.k = cb853.k LEFT JOIN cc853 ON cb853.k = cc853.k WHERE ca853.k = 3;
EXPLAIN SELECT ca853.id, cc853.id FROM ca853 LEFT JOIN cc853 ON ca853.k = cc853.k AND ca853.v = 2 WHERE ca853.k = 5;
EXPLAIN SELECT ca853.id, cc853.id FROM ca853 FULL JOIN cc853 ON ca853.k = cc853.k WHERE ca853.k = 5;
