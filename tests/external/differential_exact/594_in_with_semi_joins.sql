-- An IN or ANY subquery with its own WITH list pulls up into a semi join
-- (convert_ANY_sublink_to_join) once the WITH queries it inlines are
-- substituted; EXISTS keeps such a query as a subplan. A set operation in a
-- subquery keeps its WITH queries as InitPlans of its top node.
CREATE TABLE s594 (id int, v int);
CREATE TABLE r594 (id int);
INSERT INTO s594 SELECT g, g FROM generate_series(1, 50) g;
INSERT INTO r594 SELECT g FROM generate_series(1, 50, 5) g;
ANALYZE s594; ANALYZE r594;
EXPLAIN (COSTS OFF) SELECT * FROM s594 WHERE id IN (WITH c AS (SELECT id FROM r594) SELECT id FROM c);
SELECT * FROM s594 WHERE id IN (WITH c AS (SELECT id FROM r594) SELECT id FROM c) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT * FROM s594 WHERE id IN (WITH c AS (SELECT id FROM r594 WHERE id > 10) SELECT id FROM c WHERE id < 40);
SELECT * FROM s594 WHERE id IN (WITH c AS (SELECT id FROM r594 WHERE id > 10) SELECT id FROM c WHERE id < 40) ORDER BY id;
EXPLAIN (COSTS OFF) SELECT * FROM s594 WHERE id IN (WITH c AS (SELECT id FROM r594), d AS (SELECT id FROM c) SELECT id FROM d);
EXPLAIN (COSTS OFF) SELECT * FROM s594 WHERE id = ANY (WITH c AS (SELECT id FROM r594) SELECT id FROM c);
EXPLAIN (COSTS OFF) SELECT * FROM s594 WHERE id NOT IN (WITH c AS (SELECT id FROM r594) SELECT id FROM c);
SELECT count(*) FROM s594 WHERE id NOT IN (WITH c AS (SELECT id FROM r594) SELECT id FROM c);
EXPLAIN (COSTS OFF) SELECT * FROM s594 WHERE v > (WITH c AS MATERIALIZED (SELECT id FROM r594) SELECT max(id) FROM c UNION ALL SELECT 1 LIMIT 1);
