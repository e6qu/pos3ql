-- postgres_fdw plans a foreign partition or inheritance child as a foreign
-- table scanned on its own: the scan's conditions that ship leave its
-- Foreign Scan without a filter, and one that ships none keeps them all.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER fs614 FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER fs614 OPTIONS (user 'nobody');
CREATE TABLE fpt614 (id int, q int, note text) PARTITION BY RANGE (id);
CREATE TABLE fpt614_l PARTITION OF fpt614 FOR VALUES FROM (0) TO (10);
CREATE FOREIGN TABLE fpt614_r PARTITION OF fpt614 FOR VALUES FROM (10) TO (100) SERVER fs614 OPTIONS (schema_name 'remote', table_name 't');
EXPLAIN (COSTS OFF) SELECT * FROM fpt614;
EXPLAIN (COSTS OFF) SELECT * FROM fpt614 WHERE q > 5;
EXPLAIN (COSTS OFF) SELECT * FROM fpt614 WHERE id > 12;
EXPLAIN (COSTS OFF) SELECT * FROM fpt614 WHERE q > 5 AND note LIKE 'a%';
EXPLAIN (COSTS OFF) SELECT count(*) FROM fpt614 WHERE q = 3;
EXPLAIN (COSTS OFF) SELECT * FROM fpt614 WHERE q > random();
EXPLAIN (COSTS OFF) SELECT * FROM fpt614 x WHERE x.q IN (1, 2);
EXPLAIN (COSTS OFF) SELECT * FROM fpt614 WHERE note = 'x' OR q > random();
CREATE TABLE ifp614 (a int, v int);
CREATE FOREIGN TABLE ifp614_r () INHERITS (ifp614) SERVER fs614 OPTIONS (schema_name 'remote', table_name 'r');
EXPLAIN (COSTS OFF) SELECT * FROM ifp614 WHERE v = 1;
EXPLAIN (COSTS OFF) SELECT a FROM ifp614 WHERE a > 3 AND v < 2;
DROP SERVER fs614 CASCADE;
