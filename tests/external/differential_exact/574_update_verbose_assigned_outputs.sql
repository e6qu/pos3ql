-- EXPLAIN VERBOSE of an UPDATE prints each assigned column's planned value
-- in its scan's Output: a DEFAULT as the column's default (folded when
-- constant, a typed NULL without one), a constant folded to the column's
-- type, and an element or slice assignment as column[...] := value.
CREATE SEQUENCE c574_seq;
CREATE TABLE c574 (a int, b bigint DEFAULT 7, c numeric, d text, e int DEFAULT 5, f int, g int[] , h int DEFAULT nextval('c574_seq'), i varchar(3), j float8, k bigint);
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET b = DEFAULT;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET e = DEFAULT;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET f = DEFAULT;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET h = DEFAULT;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET b = 1;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET c = 1;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET d = 1;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET a = 1.5;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET j = 1;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET k = -3;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET a = -3;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET i = 'abcd'::text;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET g[1] = 4;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET g[1:2] = '{4,5}';
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET g[1] = DEFAULT;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE c574 SET b = 1 + 2, a = a + 1 RETURNING a;
DROP TABLE c574;
DROP SEQUENCE c574_seq;
