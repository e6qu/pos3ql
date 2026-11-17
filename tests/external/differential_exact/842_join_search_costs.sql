-- PostgreSQL's join search over plain tables without indexes: every join
-- order and method costed (initial_cost_nestloop .. final_cost_hashjoin,
-- with eqjoinsel over the columns' most common values), kept by add_path
-- and run as chosen, so rows come back in PostgreSQL's order: a hash
-- join's matches newest first, a merge join's in sorted order, and a
-- nested loop's inner rows in their scan's order.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/joinpath.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/costsize.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHash.c
CREATE TABLE a842 (id int, x int, t text);
CREATE TABLE b842 (id int, y int);
CREATE TABLE c842 (id int, z int);
CREATE TABLE d842 (k int, w int);
INSERT INTO a842 SELECT g, g % 10, 'a' || g FROM generate_series(1, 1000) g;
INSERT INTO b842 SELECT g % 500, g FROM generate_series(1, 1500) g;
INSERT INTO c842 SELECT g, g FROM generate_series(1, 50) g;
INSERT INTO d842 SELECT g % 7, g FROM generate_series(1, 300) g;
ANALYZE a842; ANALYZE b842; ANALYZE c842; ANALYZE d842;
EXPLAIN SELECT * FROM a842 JOIN b842 ON a842.id = b842.id;
EXPLAIN SELECT * FROM b842 JOIN a842 ON a842.id = b842.id;
EXPLAIN SELECT * FROM a842 JOIN c842 ON a842.id = c842.id;
EXPLAIN SELECT a842.t, b842.y FROM a842 JOIN b842 ON a842.id = b842.id WHERE a842.x = 3;
EXPLAIN SELECT * FROM a842 JOIN b842 ON a842.id = b842.id JOIN c842 ON c842.id = b842.id;
EXPLAIN SELECT * FROM a842, b842, c842, d842 WHERE a842.id = b842.id AND c842.z = d842.k AND a842.x = d842.w;
EXPLAIN SELECT * FROM c842 JOIN d842 ON c842.id = d842.k;
EXPLAIN SELECT * FROM c842, d842 WHERE c842.z < 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT * FROM a842 JOIN b842 ON a842.id = b842.id JOIN c842 ON c842.id = b842.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT * FROM c842 JOIN d842 ON c842.id = d842.k WHERE c842.z < 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT b842.y, d842.w FROM b842 JOIN d842 ON b842.id = d842.k;
SELECT * FROM a842 JOIN c842 ON a842.id = c842.id WHERE a842.x < 3;
SELECT * FROM c842 JOIN d842 ON c842.id = d842.k WHERE c842.z < 3;
SELECT b842.y, d842.w FROM b842 JOIN d842 ON b842.id = d842.k WHERE b842.y < 40;
SELECT * FROM a842 JOIN b842 ON a842.id = b842.id JOIN c842 ON c842.id = b842.id WHERE a842.x < 2;
SELECT b842.y, d842.w FROM d842 JOIN b842 ON b842.id = d842.k WHERE d842.w < 20;
SET enable_hashjoin = off;
EXPLAIN SELECT * FROM a842 JOIN b842 ON a842.id = b842.id;
EXPLAIN SELECT * FROM a842 JOIN c842 ON a842.id = c842.id;
SELECT * FROM a842 JOIN c842 ON a842.id = c842.id WHERE a842.x < 3;
SELECT * FROM c842 JOIN d842 ON c842.id = d842.k WHERE d842.w < 40;
SET enable_mergejoin = off;
EXPLAIN SELECT * FROM a842 JOIN b842 ON a842.id = b842.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT * FROM c842 JOIN d842 ON c842.id = d842.k WHERE d842.w < 40;
SELECT * FROM c842 JOIN d842 ON c842.id = d842.k WHERE d842.w < 40;
RESET enable_hashjoin;
RESET enable_mergejoin;
