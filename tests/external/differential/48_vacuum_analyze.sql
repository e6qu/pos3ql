-- Maintenance preserves data and checks targets and transaction boundaries.
-- ANALYZE samples current committed rows plus own changes. Relation estimates
-- remain visible after rollback; column and extended statistics roll back.
DROP TABLE IF EXISTS vt;
CREATE TABLE vt (a int, b text);
INSERT INTO vt VALUES (1, 'x'), (2, 'y');

VACUUM;
VACUUM vt;
VACUUM FULL vt;
VACUUM ANALYZE vt;
VACUUM (FULL, ANALYZE) vt;
VACUUM (ANALYZE) vt, vt;

ANALYZE;
ANALYZE vt;
ANALYZE vt (a, b);
ANALYZE missing_table;
ANALYZE vt (missing_column);

-- Maintenance targets retain PostgreSQL's inheritance selection. ONLY
-- touches the named relation; the default and explicit `*` include every
-- ordinary-inheritance descendant.
CREATE TABLE maintenance_parent (a integer);
CREATE TABLE maintenance_child (b integer) INHERITS (maintenance_parent);
INSERT INTO maintenance_parent VALUES (1);
INSERT INTO maintenance_child VALUES (2, 20), (3, 30);
ANALYZE ONLY maintenance_parent;
SELECT relname, reltuples::integer
  FROM pg_class WHERE relname IN ('maintenance_parent', 'maintenance_child')
  ORDER BY relname;
ANALYZE maintenance_parent *;
SELECT relname, reltuples::integer
  FROM pg_class WHERE relname IN ('maintenance_parent', 'maintenance_child')
  ORDER BY relname;
VACUUM ONLY maintenance_parent;
VACUUM maintenance_parent *;
ANALYZE ONLY maintenance_parent *;
DROP TABLE maintenance_parent CASCADE;

-- The data is untouched by maintenance.
SELECT count(*) FROM vt;

-- VACUUM cannot run inside a transaction block (25001); ANALYZE can.
BEGIN;
VACUUM vt;
ROLLBACK;
BEGIN;
ANALYZE vt;
COMMIT;

DROP TABLE vt;


CREATE TABLE analyze_pending (id integer, a integer, b integer, label text);
INSERT INTO analyze_pending VALUES (1, 1, 1, 'old'), (2, 1, 1, 'old');
CREATE STATISTICS analyze_pending_ab (ndistinct, dependencies, mcv)
ON a, b FROM analyze_pending;
ANALYZE analyze_pending;
BEGIN;
SAVEPOINT before_changes;
UPDATE analyze_pending SET a = 4, b = 4, label = NULL WHERE id = 1;
DELETE FROM analyze_pending WHERE id = 2;
INSERT INTO analyze_pending VALUES (3, 4, 4, NULL);
ANALYZE analyze_pending;
SELECT reltuples::integer FROM pg_class WHERE oid = 'analyze_pending'::regclass;
SELECT null_frac = 1 FROM pg_stats
WHERE tablename = 'analyze_pending' AND attname = 'label';
ROLLBACK TO SAVEPOINT before_changes;
SELECT null_frac = 0 FROM pg_stats
WHERE tablename = 'analyze_pending' AND attname = 'label';
ANALYZE analyze_pending(label);
COMMIT;
SELECT count(*) FROM analyze_pending;
DROP TABLE analyze_pending;
