-- An ON CONFLICT target infers every unique index PostgreSQL's
-- infer_arbiter_indexes picks: plain key columns and key expressions equal
-- to the target's as sets, a partial index only when the target's WHERE
-- implies its predicate. A row conflicts when it equals another on any
-- arbiter, and EXPLAIN lists them all. A DO UPDATE ... WHERE prints as the
-- Conflict Filter, analyzed with the rows it removed.
CREATE TABLE oc (id int, b int, t text, n int);
CREATE UNIQUE INDEX oc_id ON oc (id);
CREATE UNIQUE INDEX oc_part ON oc (b) WHERE n > 0;
CREATE UNIQUE INDEX oc_lower ON oc (lower(t));
CREATE UNIQUE INDEX oc_mixed ON oc (id, lower(t));
INSERT INTO oc VALUES (1, 1, 'A', 1), (2, 2, 'B', -1);
EXPLAIN (COSTS OFF) INSERT INTO oc VALUES (3, 1, 'c', 1) ON CONFLICT (b) WHERE n > 0 DO NOTHING;
EXPLAIN (COSTS OFF) INSERT INTO oc VALUES (3, 1, 'c', 1) ON CONFLICT (b) WHERE n > 5 DO NOTHING;
EXPLAIN (COSTS OFF) INSERT INTO oc VALUES (3, 1, 'c', 1) ON CONFLICT (id) WHERE n > 5 DO NOTHING;
EXPLAIN (COSTS OFF) INSERT INTO oc VALUES (3, 1, 'a', 1) ON CONFLICT (lower(t)) DO UPDATE SET n = 7;
EXPLAIN (COSTS OFF) INSERT INTO oc VALUES (3, 1, 'a', 1) ON CONFLICT ((lower(t))) DO NOTHING;
EXPLAIN (COSTS OFF) INSERT INTO oc VALUES (1, 1, 'a', 1) ON CONFLICT (lower(t), id) DO NOTHING;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) INSERT INTO oc VALUES (4, 1, 'a', 1) ON CONFLICT (lower(t)) DO UPDATE SET n = 7;
EXPLAIN INSERT INTO oc VALUES (5, 9, 'z', 1) ON CONFLICT (b) WHERE n > 0 DO NOTHING;
INSERT INTO oc VALUES (6, 1, 'q', 3) ON CONFLICT (b) WHERE n > 0 DO UPDATE SET t = 'p';
INSERT INTO oc VALUES (7, 2, 'r', 3) ON CONFLICT (b) WHERE n > 0 DO UPDATE SET t = 's';
INSERT INTO oc VALUES (2, 8, 'b', 1) ON CONFLICT (id, lower(t)) DO UPDATE SET n = 20;
INSERT INTO oc VALUES (2, 8, 'b', 1) ON CONFLICT (lower(t), id) DO UPDATE SET n = 30;
INSERT INTO oc VALUES (8, 1, 'w', 1) ON CONFLICT (b) DO NOTHING;
SELECT * FROM oc ORDER BY id, b;
EXPLAIN (COSTS OFF) INSERT INTO oc VALUES (1, 5, 'a', 4) ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE oc.n < excluded.n AND excluded.t <> 'z';
EXPLAIN (COSTS OFF) INSERT INTO oc AS o VALUES (1, 5, 'a', 4) ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE o.n < 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) INSERT INTO oc VALUES (1, 5, 'a', 4), (2, 5, 'b', 4), (9, 9, 'k', 1) ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE oc.n > 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON) INSERT INTO oc VALUES (1, 5, 'a', 4) ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE oc.n > 100;
SELECT * FROM oc ORDER BY id, b;
CREATE TABLE m (a int, b int, t text);
CREATE UNIQUE INDEX m_a ON m (a);
CREATE UNIQUE INDEX m_ap ON m (a) WHERE b > 0;
INSERT INTO m VALUES (1, -1, 'x');
EXPLAIN (COSTS OFF) INSERT INTO m VALUES (1, 5, 'y') ON CONFLICT (a) WHERE b > 0 DO NOTHING;
INSERT INTO m VALUES (1, 5, 'y') ON CONFLICT (a) WHERE b > 0 DO NOTHING;
INSERT INTO m VALUES (1, 5, 'y') ON CONFLICT (a) WHERE b > 0 DO UPDATE SET t = 'u';
INSERT INTO m VALUES (2, 5, 'y') ON CONFLICT (a, a) DO NOTHING;
INSERT INTO m VALUES (2, 5, 'y') ON CONFLICT (a, nope) DO NOTHING;
INSERT INTO m VALUES (2, 5, 'y') ON CONFLICT (lower(t)) DO NOTHING;
SELECT * FROM m ORDER BY a;
DROP TABLE oc, m;
