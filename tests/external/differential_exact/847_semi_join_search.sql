-- PostgreSQL's join search over the semi and anti joins pulled-up EXISTS,
-- NOT EXISTS and IN subqueries become: a semi join's paths (and a hash
-- join's right semi join, which returns each inner row at its first match),
-- its inner relation made unique by its compared columns and inner-joined
-- (create_unique_path; a sort and Unique, or none when a unique index
-- covers them), a semi join whose inner relation is unique for its clauses
-- planned as an inner join (reduce_unique_semijoins), and anti joins, each
-- sized by eqjoinsel_semi.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/analyzejoins.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/pathnode.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/selfuncs.c
CREATE TABLE sa847 (id int, x int, t text);
CREATE TABLE sb847 (id int, a_id int, y int);
CREATE TABLE sc847 (id int, k int);
INSERT INTO sa847 SELECT g, g % 10, 'a' || g FROM generate_series(1, 300) g;
INSERT INTO sb847 SELECT g, CASE WHEN g % 13 = 0 THEN NULL ELSE g % 360 END, g FROM generate_series(1, 450) g;
INSERT INTO sc847 SELECT g, g % 40 FROM generate_series(1, 60) g;
CREATE UNIQUE INDEX sc847_id ON sc847 (id);
ANALYZE sa847; ANALYZE sb847; ANALYZE sc847;
EXPLAIN SELECT sa847.id FROM sa847 WHERE EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
SELECT sa847.id FROM sa847 WHERE EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
EXPLAIN SELECT sa847.id FROM sa847 WHERE sa847.id IN (SELECT a_id FROM sb847 WHERE sb847.y < 40);
SELECT sa847.id FROM sa847 WHERE sa847.id IN (SELECT a_id FROM sb847 WHERE sb847.y < 40);
EXPLAIN SELECT sa847.id FROM sa847 WHERE NOT EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
SELECT sa847.id FROM sa847 WHERE NOT EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
EXPLAIN SELECT sa847.id FROM sa847 WHERE EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id AND sb847.y > sa847.x);
SELECT sa847.id FROM sa847 WHERE EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id AND sb847.y > sa847.x);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT sa847.id FROM sa847 WHERE EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT sa847.id FROM sa847 WHERE NOT EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
-- A unique inner relation: the semi join is an inner join.
EXPLAIN SELECT sb847.id FROM sb847 WHERE sb847.a_id IN (SELECT id FROM sc847);
SELECT sb847.id FROM sb847 WHERE sb847.a_id IN (SELECT id FROM sc847);
-- Unique-ified by hashing (its groups in the hash table's order) and
-- inner-joined.
EXPLAIN SELECT sa847.id, sa847.t FROM sa847 WHERE sa847.x IN (SELECT sc847.k FROM sc847 WHERE sc847.id < 30);
SELECT sa847.id, sa847.t FROM sa847 WHERE sa847.x IN (SELECT sc847.k FROM sc847 WHERE sc847.id < 30);
SET enable_hashjoin = off;
EXPLAIN SELECT sa847.id FROM sa847 WHERE EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
SELECT sa847.id FROM sa847 WHERE EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
RESET enable_hashjoin;
-- Unique-ified by a sort, without hashing.
SET enable_hashagg = off;
EXPLAIN SELECT sa847.id, sa847.t FROM sa847 WHERE sa847.x < 2 AND EXISTS (SELECT 1 FROM sc847 WHERE sc847.k = sa847.id);
SELECT sa847.id, sa847.t FROM sa847 WHERE sa847.x < 2 AND EXISTS (SELECT 1 FROM sc847 WHERE sc847.k = sa847.id);
RESET enable_hashagg;
-- Merge and nested-loop semi and anti joins.
SET enable_hashjoin = off;
EXPLAIN SELECT sa847.id FROM sa847 WHERE NOT EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
SELECT sa847.id FROM sa847 WHERE NOT EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
SET enable_mergejoin = off;
EXPLAIN SELECT sa847.id FROM sa847 WHERE NOT EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
SELECT sa847.id FROM sa847 WHERE NOT EXISTS (SELECT 1 FROM sb847 WHERE sb847.a_id = sa847.id);
RESET enable_hashjoin; RESET enable_mergejoin;
-- A merge semi join reads its outer input in index order.
CREATE INDEX sb847_a_id ON sb847 (a_id);
ANALYZE sb847;
SET enable_hashjoin = off; SET enable_nestloop = off;
EXPLAIN SELECT sb847.id, sb847.a_id FROM sb847 WHERE sb847.a_id IN (SELECT k FROM sc847 WHERE sc847.id < 4);
SELECT sb847.id, sb847.a_id FROM sb847 WHERE sb847.a_id IN (SELECT k FROM sc847 WHERE sc847.id < 4);
RESET enable_hashjoin; RESET enable_nestloop;
