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
