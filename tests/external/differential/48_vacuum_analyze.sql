-- VACUUM and ANALYZE. VACUUM reclaims space (a checkpoint/compaction of the
-- LSM when object storage is configured; otherwise nothing to reclaim), and
-- ANALYZE validates its targets and walks the exact live table state used by
-- this planner instead of collecting a stale sampled catalog. Both accept the
-- option and per-table/column forms, and VACUUM is non-transactional (25001)
-- while ANALYZE is not. Matches PostgreSQL 18. The VERBOSE form is omitted: it
-- prints INFO progress this engine does not.
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
