-- EXPLAIN ANALYZE of UPDATE ... FROM and DELETE ... USING with one FROM
-- table counts each node of the join PostgreSQL plans: the scans with the
-- rows their filters removed, the Hash with its table's size or the
-- Materialize with its storage, and the join with the rows its join
-- filter removed. The hash join builds on the relation PostgreSQL's costs
-- pick, a nested loop without an equality materializes or rescans the
-- relation they pick, and a node that never ran says so. VACUUM after
-- each rolled-back write keeps PostgreSQL's heap at its analyzed size.
-- The rows stay under the forced-spill run's 256 KiB memtable.
CREATE TABLE t670 (id int, v int);
INSERT INTO t670 SELECT g, g FROM generate_series(1, 500) g;
CREATE TABLE s670 (k int, w int);
INSERT INTO s670 SELECT (g % 20) * 7, g FROM generate_series(1, 40) g;
INSERT INTO s670 VALUES (NULL, 1), (NULL, 2);
CREATE TABLE big670 (k int, w int);
INSERT INTO big670 SELECT g % 1000, g FROM generate_series(1, 1500) g;
CREATE TABLE eq670 (k int, w int);
INSERT INTO eq670 SELECT g, g FROM generate_series(1, 500) g;
CREATE TABLE e670 (k int, w int);
CREATE TABLE tiny670 (k int, w int);
INSERT INTO tiny670 VALUES (3, 1), (3, 2), (5, 3);
ANALYZE t670;
ANALYZE s670;
ANALYZE big670;
ANALYZE eq670;
ANALYZE e670;
ANALYZE tiny670;
\set e 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
-- The smaller relation is hashed; on a tie the FROM table.
BEGIN; :e UPDATE t670 SET v = s670.w FROM s670 WHERE t670.id = s670.k; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE t670 SET v = big670.w FROM big670 WHERE t670.id = big670.k; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE t670 SET v = eq670.w FROM eq670 WHERE t670.id = eq670.k; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE s670 SET w = t670.v FROM t670 WHERE t670.id = s670.k; ROLLBACK; VACUUM s670;
BEGIN; :e UPDATE tiny670 SET w = t670.v FROM t670 WHERE t670.id = tiny670.k; ROLLBACK; VACUUM tiny670;
-- Each relation's own conditions filter its scan, the rest the join.
BEGIN; :e UPDATE t670 SET v = s670.w FROM s670
  WHERE t670.id = s670.k AND t670.v + s670.w > 50 AND s670.w > 3 AND t670.v < 450; ROLLBACK; VACUUM t670;
BEGIN; :e DELETE FROM t670 USING big670 WHERE t670.id = big670.k AND big670.w < 100; ROLLBACK; VACUUM t670;
BEGIN; :e DELETE FROM t670 USING big670 WHERE t670.id = big670.k AND big670.w < 450; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE t670 SET v = s670.w FROM s670 WHERE t670.id = s670.k AND t670.v = 14; ROLLBACK; VACUUM t670;
BEGIN; :e DELETE FROM t670 USING s670 WHERE t670.id = s670.k AND (t670.v > 100 OR s670.w < 10); ROLLBACK; VACUUM t670;
-- Without an equality, a nested loop.
BEGIN; :e UPDATE t670 SET v = s670.w FROM s670 WHERE t670.v < s670.k; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE t670 SET v = s670.w FROM s670 WHERE t670.v < s670.k AND t670.id < 10; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE s670 SET w = 0 FROM t670 WHERE t670.v < s670.k AND t670.id < 300; ROLLBACK; VACUUM s670;
BEGIN; :e UPDATE t670 SET v = 0 FROM tiny670; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE t670 SET v = 0 FROM e670 WHERE t670.v < e670.k; ROLLBACK; VACUUM t670;
-- Empty relations and relations their filters empty.
BEGIN; :e UPDATE t670 SET v = e670.w FROM e670 WHERE t670.id = e670.k; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE e670 SET w = t670.v FROM t670 WHERE t670.id = e670.k; ROLLBACK; VACUUM e670;
BEGIN; :e UPDATE t670 SET v = s670.w FROM s670 WHERE t670.id = s670.k AND s670.w > 500; ROLLBACK; VACUUM t670;
BEGIN; :e UPDATE t670 SET v = s670.w FROM s670 WHERE t670.id = s670.k AND t670.v > 2500; ROLLBACK; VACUUM t670;
BEGIN; :e DELETE FROM t670 USING s670 WHERE t670.v < s670.k AND t670.id > 2500; ROLLBACK; VACUUM t670;
-- RETURNING counts on the write's node.
BEGIN; :e UPDATE t670 SET v = s670.w FROM s670 WHERE t670.id = s670.k RETURNING t670.id, s670.w; ROLLBACK; VACUUM t670;
BEGIN; :e DELETE FROM t670 AS a USING s670 AS b WHERE a.id = b.k RETURNING a.*, b.*; ROLLBACK; VACUUM t670;
-- Each target row changes once, with the FROM row of its first join row,
-- in the join's order.
UPDATE t670 SET v = tiny670.w FROM tiny670 WHERE t670.id = tiny670.k RETURNING t670.id, t670.v, tiny670.*;
UPDATE tiny670 SET w = t670.v FROM t670 WHERE t670.id = tiny670.k RETURNING tiny670.*, t670.id;
UPDATE t670 SET v = big670.w FROM big670 WHERE t670.id = big670.k AND t670.id < 10 RETURNING t670.*, big670.*;
UPDATE s670 SET w = t670.v FROM t670 WHERE t670.v < s670.k AND t670.id < 300 RETURNING s670.*, t670.id;
UPDATE t670 SET v = s670.w FROM s670 WHERE t670.v < s670.k AND t670.id < 4 RETURNING t670.id, t670.v, s670.k;
DELETE FROM t670 USING eq670 WHERE t670.id = eq670.k AND eq670.w BETWEEN 20 AND 25 RETURNING t670.id, eq670.w;
SELECT id, v FROM t670 WHERE v <> id ORDER BY id;
