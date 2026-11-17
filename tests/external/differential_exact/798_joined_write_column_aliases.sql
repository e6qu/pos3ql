-- A joined `UPDATE` or `DELETE` whose FROM table is read under column
-- aliases plans its join as PostgreSQL does, and EXPLAIN prints the join's
-- conditions with the aliased names (the range table entry's `eref`).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/ruleutils.c
CREATE TABLE t798 (id int, v int, note text);
CREATE TABLE s798 (k int, w int, tag text);
INSERT INTO t798 SELECT g, g * 2, 'n' || g FROM generate_series(1, 300) g;
INSERT INTO s798 SELECT g, g % 7, 't' || (g % 3) FROM generate_series(1, 200) g;
ANALYZE t798;
ANALYZE s798;
EXPLAIN (COSTS OFF) UPDATE t798 SET v = x.w2 FROM s798 AS x(k2, w2) WHERE t798.id = x.k2 AND x.w2 > 2;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE t798 SET v = x.w2 FROM s798 AS x(k2, w2) WHERE t798.id = x.k2 AND x.w2 > 2;
EXPLAIN (COSTS OFF) DELETE FROM t798 USING s798 x(k2) WHERE id = k2 + 1 AND tag = 't1';
UPDATE t798 SET v = x.w2 FROM s798 AS x(k2, w2) WHERE t798.id = x.k2 AND x.w2 > 5 AND x.k2 < 60 RETURNING t798.id, x.k2, x.w2, x.tag;
DELETE FROM t798 USING s798 x(k2) WHERE id = k2 + 1 AND tag = 't1' AND k2 < 30 RETURNING id, k2, w;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) UPDATE t798 SET v = x.w2 FROM s798 AS x(k2, w2) WHERE t798.id = x.k2 AND x.w2 > 5;
SELECT count(*), sum(v) FROM t798;
