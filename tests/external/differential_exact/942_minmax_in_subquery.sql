-- min and max in an uncorrelated subquery are planned as
-- preprocess_minmax_aggregates plans them: the subquery's InitPlan reads a
-- Limit over an ordered index scan, numbered before the subquery's own.
CREATE TABLE mm942 (id int PRIMARY KEY, k int, v text);
INSERT INTO mm942 SELECT i, i % 100, 'v' || i FROM generate_series(1, 3000) i;
CREATE INDEX mm942_k ON mm942 (k);
ANALYZE mm942;
EXPLAIN (COSTS OFF) SELECT * FROM mm942 WHERE id = (SELECT max(id) FROM mm942);
EXPLAIN SELECT * FROM mm942 WHERE id = (SELECT max(id) FROM mm942);
EXPLAIN (COSTS OFF) SELECT id FROM mm942 WHERE k = (SELECT min(k) FROM mm942 WHERE id > 10) LIMIT 2;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT * FROM mm942 WHERE id = (SELECT max(id) FROM mm942);
SELECT * FROM mm942 WHERE id = (SELECT max(id) FROM mm942);
SELECT id FROM mm942 WHERE k = (SELECT min(k) FROM mm942 WHERE id > 10) ORDER BY id LIMIT 2;
EXPLAIN (COSTS OFF) SELECT (SELECT max(id) FROM mm942), (SELECT min(k) FROM mm942);
EXPLAIN SELECT id, (SELECT count(*) FROM mm942) FROM mm942 WHERE id < 3;
DROP TABLE mm942;
