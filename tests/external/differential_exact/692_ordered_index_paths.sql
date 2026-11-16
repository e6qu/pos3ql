-- ORDER BY and LIMIT weigh an index that returns rows in the requested
-- order (forward, or backward for the reverse order) against the cheapest
-- path under a sort, and a LIMIT weighs each path by the fraction of its
-- rows it reads, as PostgreSQL's add_path, create_ordered_paths and
-- create_limit_path do. A row limit then returns the rows of the path
-- chosen. The rows stay under the forced-spill run's 256 KiB memtable.
CREATE TABLE o692 (id int PRIMARY KEY, k int, v int);
INSERT INTO o692 SELECT g, g % 100, (g * 37) % 3000 FROM generate_series(1, 3000) g;
CREATE INDEX o692_k ON o692 (k);
CREATE INDEX o692_v ON o692 (v DESC);
ANALYZE o692;
\set e 'EXPLAIN (COSTS OFF)'
:e SELECT * FROM o692 ORDER BY id;
:e SELECT * FROM o692 ORDER BY id LIMIT 5;
:e SELECT * FROM o692 ORDER BY id DESC LIMIT 5;
:e SELECT * FROM o692 ORDER BY k;
:e SELECT * FROM o692 ORDER BY k LIMIT 5;
:e SELECT * FROM o692 ORDER BY k LIMIT 500;
:e SELECT * FROM o692 ORDER BY v DESC LIMIT 3;
:e SELECT * FROM o692 ORDER BY v LIMIT 3;
:e SELECT * FROM o692 ORDER BY k NULLS FIRST LIMIT 3;
:e SELECT * FROM o692 WHERE k = 5 ORDER BY id;
:e SELECT * FROM o692 WHERE k = 5 ORDER BY k;
:e SELECT * FROM o692 WHERE id < 100 ORDER BY id;
:e SELECT * FROM o692 WHERE id < 100 ORDER BY id DESC;
:e SELECT * FROM o692 WHERE k < 3 LIMIT 4;
:e SELECT * FROM o692 WHERE v > 2900 ORDER BY id LIMIT 2;
:e SELECT id FROM o692 ORDER BY id LIMIT 10 OFFSET 20;
:e SELECT * FROM o692 LIMIT 3;
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM o692 ORDER BY id DESC LIMIT 5;
:a SELECT * FROM o692 WHERE k < 3 ORDER BY k LIMIT 4;
-- The limited rows are the chosen path's first rows.
SELECT id, k FROM o692 WHERE k < 3 LIMIT 4;
SELECT id, v FROM o692 ORDER BY v LIMIT 5;
SELECT id, v FROM o692 ORDER BY v DESC LIMIT 5;
SELECT id, k FROM o692 ORDER BY k LIMIT 5;
SELECT id, k FROM o692 ORDER BY k DESC LIMIT 5;
SELECT id FROM o692 ORDER BY id DESC LIMIT 3 OFFSET 2;
