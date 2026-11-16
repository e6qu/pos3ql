-- UPDATE ... FROM and DELETE ... USING hash on any equality between an
-- expression of the target and one of the FROM table, and treat a routine's
-- parameters and variables and built-in function calls in the conditions as
-- PostgreSQL's planner does: the rows change in the join's order, which
-- RETURNING, triggers and routine callers see.
CREATE TABLE t740 (id integer, v text);
CREATE TABLE s740 (k integer, w text);
INSERT INTO t740 SELECT g, 'v' || g FROM generate_series(1, 10) g;
INSERT INTO s740 SELECT (g * 7) % 101, 'w' || g FROM generate_series(1, 100) g;
ANALYZE t740;
ANALYZE s740;
EXPLAIN (COSTS OFF) UPDATE t740 SET v = s740.w FROM s740 WHERE t740.id = s740.k + 1;
\set e 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
BEGIN;
:e UPDATE t740 SET v = s740.w FROM s740 WHERE t740.id = s740.k + 1;
ROLLBACK;
BEGIN;
UPDATE t740 SET v = s740.w FROM s740 WHERE t740.id = s740.k + 1 RETURNING t740.id, s740.k, t740.v;
ROLLBACK;
BEGIN;
UPDATE t740 SET v = s740.w FROM s740 WHERE s740.k * 2 = t740.id * 2 AND t740.id > 2 RETURNING t740.id;
ROLLBACK;
EXPLAIN (COSTS OFF) DELETE FROM t740 USING s740 WHERE t740.id = s740.k AND abs(s740.k - 50) > 45;
BEGIN;
DELETE FROM t740 USING s740 WHERE t740.id = s740.k AND abs(s740.k - 50) > 45 RETURNING t740.id;
ROLLBACK;
CREATE FUNCTION f740(p integer) RETURNS SETOF integer LANGUAGE sql
  AS $$ UPDATE t740 SET v = s740.w FROM s740 WHERE t740.id = s740.k AND s740.k > p RETURNING t740.id $$;
BEGIN;
SELECT * FROM f740(2);
ROLLBACK;
CREATE FUNCTION g740(p integer) RETURNS SETOF integer LANGUAGE sql
  AS $$ DELETE FROM t740 USING s740 WHERE t740.id = s740.k + $1 RETURNING t740.id $$;
BEGIN;
SELECT * FROM g740(1);
ROLLBACK;
DO $$
DECLARE lim integer := 2; r record;
BEGIN
  FOR r IN UPDATE t740 SET v = s740.w FROM s740 WHERE t740.id = s740.k AND s740.k > lim RETURNING t740.id LOOP
    RAISE NOTICE '%', r.id;
  END LOOP;
END $$;
-- A key of a type the join cannot hash, and one equated with a constant.
BEGIN;
UPDATE t740 SET v = s740.w FROM s740 WHERE t740.id = s740.k + 1 AND s740.k + 1 = 4 RETURNING t740.id;
ROLLBACK;
