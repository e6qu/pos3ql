-- EXPLAIN EXECUTE shows the plan the execution takes (GetCachedPlan): a
-- custom plan folds the arguments into its conditions, a generic plan
-- prints $n. Under plan_cache_mode = auto the first five executions get
-- custom plans and later ones the generic plan; a statement without
-- parameters always runs its generic plan. EXPLAIN EXECUTE counts too.
CREATE TABLE p592 (id int, v int, s text);
INSERT INTO p592 SELECT g, g, 's' || g FROM generate_series(1, 100) g;
ANALYZE p592;
PREPARE e592(int) AS SELECT * FROM p592 WHERE v = $1;
EXPLAIN (COSTS OFF) EXECUTE e592(5);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE e592(5);
PREPARE c592(int, text) AS SELECT id FROM p592 WHERE v > $1 AND $2 = 'x';
EXPLAIN (COSTS OFF) EXECUTE c592(98, 'x');
EXPLAIN (COSTS OFF) EXECUTE c592(98, 'y');
PREPARE b592(bigint) AS SELECT id FROM p592 WHERE v = $1;
EXPLAIN (COSTS OFF) EXECUTE b592(3);
PREPARE t592(text) AS SELECT id FROM p592 WHERE s = $1 OR v::text = $1;
EXPLAIN (COSTS OFF) EXECUTE t592('s3');
SET plan_cache_mode = force_generic_plan;
EXPLAIN (COSTS OFF) EXECUTE e592(5);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE e592(5);
EXPLAIN (COSTS OFF) EXECUTE c592(98, 'x');
EXPLAIN (COSTS OFF) EXECUTE b592(3);
EXPLAIN (COSTS OFF) EXECUTE t592('s3');
SET plan_cache_mode = force_custom_plan;
EXPLAIN (COSTS OFF) EXECUTE e592(7);
RESET plan_cache_mode;
PREPARE a592(int) AS SELECT id FROM p592 WHERE v = $1;
EXECUTE a592(1);
EXECUTE a592(2);
EXPLAIN (COSTS OFF) EXECUTE a592(3);
EXECUTE a592(4);
EXECUTE a592(5);
EXPLAIN (COSTS OFF) EXECUTE a592(6);
EXECUTE a592(7);
PREPARE n592 AS SELECT count(*) FROM p592;
EXECUTE n592;
EXPLAIN (COSTS OFF) EXECUTE n592;
SELECT name, generic_plans, custom_plans FROM pg_prepared_statements ORDER BY name;
