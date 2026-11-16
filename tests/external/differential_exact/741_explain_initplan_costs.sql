-- The query's top node pays for running its InitPlans once
-- (`SS_charge_for_initplans`): an InitPlan costs its plan's total cost, an
-- `EXISTS` test its startup cost and the run cost of one row. A gating
-- `Result` takes the costs of the plan it gates, and an `EXISTS` subquery
-- returns no columns (`simplify_EXISTS_query`).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/subselect.c
CREATE TABLE a741 (id integer PRIMARY KEY, v integer);
INSERT INTO a741 SELECT g, g % 7 FROM generate_series(1, 1000) g;
CREATE TABLE b741 (x integer, y integer);
INSERT INTO b741 SELECT g, g FROM generate_series(1, 300) g;
ANALYZE a741;
ANALYZE b741;
EXPLAIN SELECT * FROM a741 WHERE id = 3 AND EXISTS (SELECT 1 FROM b741 WHERE x = 3);
EXPLAIN SELECT * FROM a741 WHERE id = 4 AND (SELECT y FROM b741 WHERE x = 3) > 0;
EXPLAIN SELECT * FROM a741 WHERE id = 4 AND (SELECT y FROM b741 WHERE x = 3) > 0 AND EXISTS (SELECT 1 FROM b741 WHERE y = 2);
EXPLAIN SELECT * FROM a741 WHERE NOT EXISTS (SELECT 1 FROM b741 WHERE x < 0) AND id = 9;
EXPLAIN (COSTS OFF) SELECT * FROM a741 WHERE id = 3 AND EXISTS (SELECT 1 FROM b741 WHERE x = 3);
SELECT * FROM a741 WHERE id = 3 AND EXISTS (SELECT 1 FROM b741 WHERE x = 3);
