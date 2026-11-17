-- PostgreSQL's join search over tables with btree indexes: each relation's
-- index paths, sorted ones feeding merge joins, parameterized index scans
-- inside nested loops (costed per outer row with get_loop_count and the
-- Mackert and Lohman estimate), a unique inner relation's costs
-- (innerrel_is_unique, with the join's semijoin factors) and Memoize over a
-- parameterized scan (cost_memoize_rescan), each run as chosen.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/indxpath.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/joinpath.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/analyzejoins.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeMemoize.c
CREATE TABLE a844 (id int PRIMARY KEY, x int, t text);
CREATE TABLE b844 (id int, a_id int, y int);
CREATE TABLE c844 (k int, z int);
CREATE TABLE d844 (k int, name text);
INSERT INTO a844 SELECT g, g % 10, 'a' || g FROM generate_series(1, 1000) g;
INSERT INTO b844 SELECT g, g % 500, g FROM generate_series(1, 1500) g;
INSERT INTO c844 SELECT g % 50, g FROM generate_series(1, 200) g;
INSERT INTO d844 SELECT g, 'd' || g FROM generate_series(1, 1200) g;
CREATE INDEX b844_a_id ON b844 (a_id);
CREATE INDEX c844_k ON c844 (k);
CREATE INDEX d844_k ON d844 (k);
ANALYZE a844; ANALYZE b844; ANALYZE c844; ANALYZE d844;
-- A unique inner relation: hash joins and nested loops stop at each outer
-- row's first match.
EXPLAIN SELECT * FROM a844 JOIN b844 ON a844.id = b844.a_id;
EXPLAIN SELECT * FROM b844 JOIN a844 ON a844.id = b844.a_id WHERE b844.y < 100;
EXPLAIN SELECT * FROM a844 JOIN b844 ON a844.id = b844.a_id WHERE b844.y < 10;
EXPLAIN SELECT * FROM a844 JOIN c844 ON a844.id = c844.k;
EXPLAIN SELECT * FROM a844 JOIN b844 ON a844.id = b844.a_id JOIN c844 ON c844.k = b844.a_id;
SELECT a844.t, b844.y FROM a844 JOIN b844 ON a844.id = b844.a_id WHERE b844.y < 10;
SELECT * FROM b844 JOIN a844 ON a844.id = b844.a_id WHERE b844.y < 40;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT * FROM a844 JOIN b844 ON a844.id = b844.a_id WHERE b844.y < 10;
-- Memoize over a parameterized index scan of repeated keys.
CREATE TABLE f844 (id int, k int, v int);
INSERT INTO f844 SELECT g, g % 20, g FROM generate_series(1, 1000) g;
ANALYZE f844;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN SELECT f844.id, d844.name FROM f844 JOIN d844 ON f844.k = d844.k;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT f844.id, d844.name FROM f844 JOIN d844 ON f844.k = d844.k WHERE f844.id < 30;
SELECT f844.id, d844.name FROM f844 JOIN d844 ON f844.k = d844.k WHERE f844.id < 30;
SET enable_memoize = off;
EXPLAIN SELECT f844.id, d844.name FROM f844 JOIN d844 ON f844.k = d844.k;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT f844.id, d844.name FROM f844 JOIN d844 ON f844.k = d844.k WHERE f844.id < 30;
RESET enable_memoize;
RESET enable_mergejoin;
-- Index order feeding a merge join.
EXPLAIN SELECT * FROM a844 JOIN b844 ON a844.id = b844.a_id;
EXPLAIN SELECT * FROM c844 JOIN b844 ON c844.k = b844.a_id;
SELECT c844.z, b844.y FROM c844 JOIN b844 ON c844.k = b844.a_id WHERE c844.z < 12;
RESET enable_hashjoin;
SET enable_hashjoin = off;
SET enable_nestloop = off;
EXPLAIN SELECT * FROM a844 JOIN d844 ON a844.id = d844.k;
SELECT a844.t, d844.name FROM a844 JOIN d844 ON a844.id = d844.k WHERE a844.x = 3 AND a844.id < 100;
RESET enable_hashjoin;
RESET enable_nestloop;
-- A merge join's inputs are read as far as it fetches them: an index scan
-- in order stops past the outer's last key, and its sort reports its memory.
SET enable_hashjoin = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT * FROM a844 JOIN c844 ON a844.id = c844.k WHERE c844.z < 120;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT c844.z, b844.y FROM c844 JOIN b844 ON c844.k = b844.a_id;
RESET enable_hashjoin;
-- A link table whose key joins two relations: its scan parameterized by
-- both makes a parameterized join (allow_star_schema_join).
CREATE TABLE la844 (id int PRIMARY KEY, n text);
CREATE TABLE lb844 (id int PRIMARY KEY, m text);
CREATE TABLE lab844 (a_id int, b_id int);
INSERT INTO la844 SELECT g, 'a' || g FROM generate_series(1, 300) g;
INSERT INTO lb844 SELECT g, 'b' || g FROM generate_series(1, 200) g;
INSERT INTO lab844 SELECT g % 300 + 1, (g / 300) % 200 + 1 FROM generate_series(1, 1200) g;
ALTER TABLE lab844 ADD PRIMARY KEY (a_id, b_id);
ANALYZE la844; ANALYZE lb844; ANALYZE lab844;
EXPLAIN SELECT la844.n, lb844.m FROM la844 JOIN lab844 ON lab844.a_id = la844.id
  JOIN lb844 ON lb844.id = lab844.b_id WHERE la844.id < 5 AND lb844.id < 50;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN SELECT la844.n, lb844.m FROM la844 JOIN lab844 ON lab844.a_id = la844.id
  JOIN lb844 ON lb844.id = lab844.b_id WHERE la844.id < 20;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT la844.n, lb844.m FROM la844 JOIN lab844 ON lab844.a_id = la844.id
  JOIN lb844 ON lb844.id = lab844.b_id WHERE la844.id < 4;
SELECT la844.n, lb844.m FROM la844 JOIN lab844 ON lab844.a_id = la844.id
  JOIN lb844 ON lb844.id = lab844.b_id WHERE la844.id < 4;
RESET enable_hashjoin;
RESET enable_mergejoin;
