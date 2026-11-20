-- DISTINCT with an ORDER BY on some of its columns: the sort keys are the
-- ORDER BY items then the remaining output columns, so tied rows come back
-- in the order of the rest. Hashing is off so PostgreSQL plans Sort over
-- Unique.
CREATE TABLE d923 (id int PRIMARY KEY, c text, flag bool, n int);
INSERT INTO d923 SELECT i, 'c' || i, i % 2 = 0, i % 3 FROM generate_series(0, 29) i;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT DISTINCT c, flag FROM d923 ORDER BY flag;
SELECT DISTINCT c, flag FROM d923 ORDER BY flag;
SELECT DISTINCT c, flag FROM d923 ORDER BY flag DESC;
SELECT DISTINCT flag, c FROM d923 ORDER BY flag;
SELECT DISTINCT c, n, flag FROM d923 ORDER BY n, flag DESC;
SELECT DISTINCT n, c FROM d923 ORDER BY n DESC LIMIT 7;
SELECT DISTINCT n FROM d923 ORDER BY n;
