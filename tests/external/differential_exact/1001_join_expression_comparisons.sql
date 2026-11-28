-- A join condition comparing one relation's column with another's
-- expression (`<>`, `<`, ...) is planned by the join search, estimated by
-- `neqjoinsel` over the expression's default distinct count or by
-- `scalarltjoinsel`, and a Memoize keys by its outer expression.
CREATE TABLE ao (id int, k int);
CREATE TABLE ai (k int, v int);
INSERT INTO ao SELECT g, g % 50 FROM generate_series(1, 400) g;
INSERT INTO ai SELECT g % 60, g FROM generate_series(1, 300) g;
CREATE INDEX ai_k ON ai (k);
ANALYZE ao, ai;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM ao JOIN ai ON ai.k = ao.k AND ai.v <> ao.id % 7;
EXPLAIN SELECT count(*) FROM ao JOIN ai ON ai.k = ao.k AND ai.v <> ao.id % 7;
SELECT count(*) FROM ao JOIN ai ON ai.k = ao.k AND ai.v <> ao.id % 7;
EXPLAIN SELECT ao.id, ai.v FROM ao JOIN ai ON ai.k = ao.k AND ai.v < ao.id * 2;
SELECT count(*) FROM ao JOIN ai ON ai.k = ao.k AND ai.v < ao.id * 2;
RESET enable_nestloop;
EXPLAIN SELECT count(*) FROM ao JOIN ai ON ai.k = ao.k AND ai.v <> ao.id % 7;
EXPLAIN SELECT count(*) FROM ao WHERE NOT EXISTS (SELECT FROM ai WHERE ai.k = ao.k AND ai.v <> ao.id % 7);
SELECT count(*) FROM ao WHERE NOT EXISTS (SELECT FROM ai WHERE ai.k = ao.k AND ai.v <> ao.id % 7);
EXPLAIN SELECT count(*) FROM ao WHERE EXISTS (SELECT FROM ai WHERE ai.k = ao.k AND ai.v + 1 >= ao.id);
SELECT count(*) FROM ao WHERE EXISTS (SELECT FROM ai WHERE ai.k = ao.k AND ai.v + 1 >= ao.id);
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN SELECT count(*) FROM ao JOIN ai ON ai.k = ao.k AND ai.v <> ao.id % 7;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM ao JOIN ai ON ai.k = ao.k AND ai.v <> ao.id % 7;
SELECT ao.id, ai.v FROM ao JOIN ai ON ai.k = ao.k AND ai.v <> ao.id % 7 ORDER BY 1, 2 LIMIT 5;
RESET ALL;
