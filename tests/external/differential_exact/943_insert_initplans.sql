-- An INSERT whose source PostgreSQL pulls up runs the source's InitPlans in
-- its own node, which pays for them; a source kept as a subquery keeps them.
-- One VALUES row's subqueries are InitPlans of the Insert in column order.
-- The source's top node computes the target list coerced to the columns.
CREATE TABLE is943 (id int PRIMARY KEY, k int);
CREATE TABLE it943 (id int, k bigint);
INSERT INTO is943 SELECT i, i % 10 FROM generate_series(1, 100) i;
ANALYZE is943;
EXPLAIN (COSTS OFF) INSERT INTO it943 SELECT id, k FROM is943 WHERE k = (SELECT max(k) FROM is943);
EXPLAIN INSERT INTO it943 SELECT id, (SELECT count(*) FROM is943) FROM is943 WHERE id < 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) INSERT INTO it943 SELECT id, (SELECT count(*) FROM is943) FROM is943 WHERE id < 3;
EXPLAIN (COSTS OFF) INSERT INTO it943 VALUES (1, (SELECT count(*) FROM is943));
EXPLAIN (COSTS OFF) INSERT INTO it943 (k, id) VALUES ((SELECT count(*) FROM is943), (SELECT min(k) FROM is943));
EXPLAIN (COSTS OFF) INSERT INTO it943 SELECT id, k FROM is943 WHERE k = (SELECT max(k) FROM is943) ORDER BY id LIMIT 2;
EXPLAIN (COSTS OFF) INSERT INTO it943 SELECT 1, (SELECT count(*) FROM is943);
EXPLAIN (COSTS OFF) INSERT INTO it943 SELECT id, k FROM is943 WHERE k = (SELECT max(k) FROM is943) RETURNING id;
EXPLAIN INSERT INTO it943 SELECT id, 5 FROM is943 WHERE id < 3;
SELECT count(*) FROM it943;
DROP TABLE is943, it943;
