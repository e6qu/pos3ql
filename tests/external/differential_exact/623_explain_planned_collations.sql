-- COLLATE as the planner leaves it: a constant carries the collation
-- (`'a'::text COLLATE "C"`), any other operand is relabeled, which shows as
-- a cast where implicit coercions print, and a sort key then names the
-- collation. A foreign scan keeps such conditions and orders local.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER pc_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER pc_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE pc_f (id int, note text, v varchar(10), c char(3))
  SERVER pc_s OPTIONS (schema_name 'remote', table_name 't');
CREATE TABLE pc_l (id int, note text, v varchar(10), c char(3));
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_f WHERE note = 'n1' COLLATE "C";
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_f ORDER BY note COLLATE "C" LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note COLLATE "C" FROM pc_f;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_f WHERE v COLLATE "C" > 'n1';
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_f ORDER BY c COLLATE "POSIX" DESC;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_f WHERE (note || 'x') COLLATE "C" > 'a';
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_f WHERE note COLLATE "C" IN ('a', 'b');
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_l WHERE note = 'n1' COLLATE "C";
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_l ORDER BY note COLLATE "C";
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_l ORDER BY v COLLATE "C";
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_l WHERE v COLLATE "C" > 'n1';
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_l ORDER BY note COLLATE "POSIX", id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT note COLLATE "C" AS k FROM pc_l ORDER BY k;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_l ORDER BY note COLLATE "default";
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_l WHERE upper(note COLLATE "C") > 'a';
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pc_l WHERE note > 'a' COLLATE "C";
EXPLAIN (VERBOSE, COSTS OFF) SELECT note COLLATE "C", count(*) FROM pc_l GROUP BY note COLLATE "C";
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT note COLLATE "C" FROM pc_l;
EXPLAIN (COSTS OFF) SELECT id FROM pc_l ORDER BY note COLLATE "C", v COLLATE "POSIX" DESC NULLS LAST;
EXPLAIN (COSTS OFF) SELECT id FROM pc_l WHERE note COLLATE "C" > 'n1';
DROP TABLE pc_l;
DROP SERVER pc_s CASCADE;
