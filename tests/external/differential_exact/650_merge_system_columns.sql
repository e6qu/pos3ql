-- A MERGE reads its target's and a stored source's tableoid in conditions,
-- values and RETURNING, as UPDATE and DELETE do: the target row's table (a
-- partition's own), or the table a new row lands in. A WHEN condition reads
-- no other system column (42P10), and an action reads only the relations
-- its kind reaches: a NOT MATCHED action the source, a NOT MATCHED BY SOURCE
-- action the target (42P01, or 42703 for a bare column).
CREATE TABLE t650 (id int, v int);
INSERT INTO t650 SELECT g, g FROM generate_series(1, 5) g;
CREATE TABLE s650 (k int, w int);
INSERT INTO s650 VALUES (1, 10), (2, 20), (9, 90);
CREATE TABLE p650 (id int, v int) PARTITION BY RANGE (id);
CREATE TABLE p650_1 PARTITION OF p650 FOR VALUES FROM (0) TO (3);
CREATE TABLE p650_2 PARTITION OF p650 FOR VALUES FROM (3) TO (100);
INSERT INTO p650 SELECT g, g FROM generate_series(1, 5) g;
ANALYZE t650;
ANALYZE s650;
ANALYZE p650;
BEGIN;
MERGE INTO t650 USING s650 ON t650.id = s650.k
  WHEN MATCHED AND t650.tableoid = 't650'::regclass THEN UPDATE SET v = length(t650.tableoid::regclass::text)
  WHEN NOT MATCHED BY SOURCE AND t650.tableoid::regclass::text = 't650' THEN DELETE
  WHEN NOT MATCHED AND s650.tableoid = 's650'::regclass THEN INSERT VALUES (s650.k, s650.w)
  RETURNING merge_action(), t650.tableoid::regclass, s650.tableoid::regclass,
    old.tableoid::regclass, new.tableoid::regclass, t650.*;
ROLLBACK;
BEGIN;
MERGE INTO p650 USING s650 ON p650.id = s650.k
  WHEN MATCHED THEN UPDATE SET v = length(p650.tableoid::regclass::text)
  WHEN NOT MATCHED BY SOURCE AND p650.tableoid = 'p650_2'::regclass THEN DELETE
  WHEN NOT MATCHED THEN INSERT VALUES (s650.k, s650.w)
  RETURNING merge_action(), p650.tableoid::regclass, p650.*;
ROLLBACK;
BEGIN;
MERGE INTO t650 t USING s650 s ON t.id = s.k AND s.tableoid <> 0
  WHEN MATCHED AND s.tableoid = 's650'::regclass THEN UPDATE SET v = 0
  WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.tableoid::int / s.tableoid::int)
  RETURNING merge_action(), s.tableoid::regclass, t.tableoid::regclass, s;
ROLLBACK;
BEGIN;
MERGE INTO t650 USING (VALUES (1, 2)) v(k, w) ON t650.id = v.k
  WHEN MATCHED THEN UPDATE SET v = 0
  RETURNING tableoid::regclass;
UPDATE p650 SET v = length(p650.tableoid::regclass::text) FROM s650
  WHERE p650.id = s650.k AND p650.tableoid <> 0 AND s650.tableoid <> 0
  RETURNING p650.tableoid::regclass, s650.tableoid::regclass, p650.*;
DELETE FROM p650 USING s650 WHERE p650.id = s650.k + 2 AND p650.tableoid = 'p650_2'::regclass
  RETURNING p650.tableoid::regclass, p650.*;
ROLLBACK;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN MATCHED THEN UPDATE SET v = tableoid::int;
MERGE INTO t650 USING (VALUES (1, 2)) v(k, w) ON t650.id = v.k WHEN MATCHED THEN UPDATE SET v = v.tableoid::int;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN MATCHED THEN UPDATE SET ctid = '(0,1)';
UPDATE t650 SET xmin = '1';
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED THEN INSERT (tableoid) VALUES (1);
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED THEN INSERT VALUES (t650.id);
MERGE INTO t650 t USING s650 ON t.id = s650.k WHEN NOT MATCHED THEN INSERT VALUES (t.id);
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED THEN INSERT VALUES (id);
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED AND t650.xmin = 0 THEN DO NOTHING;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED AND t650.tableoid = 0 THEN DO NOTHING;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED BY SOURCE AND s650.w = 1 THEN DELETE;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED BY SOURCE AND w = 1 THEN DELETE;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED BY SOURCE THEN UPDATE SET v = s650.w;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED BY SOURCE AND s650.* IS NULL THEN DELETE;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN MATCHED AND t650.ctid = '(0,1)' THEN DELETE;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN MATCHED AND s650.xmin = '1' THEN DELETE;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED AND s650.cmax = '1' THEN DO NOTHING;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN MATCHED AND t650.v = 1 AND t650.nosuch = 1 THEN DELETE;
MERGE INTO t650 USING s650 ON t650.id = s650.k WHEN NOT MATCHED AND s650.nosuch = 1 THEN DO NOTHING;
