-- PostgreSQL 18 advisory locks: key namespaces, reentrancy, lock modes,
-- transaction/savepoint ownership, backend identity, and exact catalogs.

SELECT pg_backend_pid() > 0,
       pg_blocking_pids(pg_backend_pid()) = ARRAY[]::integer[];
SHOW max_locks_per_transaction;

SELECT pg_try_advisory_lock(4294967298);
SELECT locktype, database IS NOT NULL, relation IS NULL, classid, objid,
       objsubid, mode, granted, fastpath, pid = pg_backend_pid(),
       waitstart IS NULL
  FROM pg_locks
 WHERE locktype = 'advisory';

-- Session locks count repeated acquisitions but expose one pg_locks row.
SELECT pg_try_advisory_lock(4294967298);
SELECT count(*) FROM pg_locks
 WHERE locktype = 'advisory' AND classid = 1 AND objid = 2
   AND objsubid = 1 AND mode = 'ExclusiveLock';
SELECT pg_advisory_unlock(4294967298);
SELECT count(*) FROM pg_locks
 WHERE locktype = 'advisory' AND classid = 1 AND objid = 2
   AND objsubid = 1 AND mode = 'ExclusiveLock';
SELECT pg_advisory_unlock(4294967298), pg_advisory_unlock(4294967298);

-- A bigint and an integer pair with the same 64 bits are distinct namespaces.
SELECT pg_try_advisory_lock(1), pg_try_advisory_lock(0, 1);
SELECT classid, objid, objsubid, mode
  FROM pg_locks
 WHERE locktype = 'advisory'
 ORDER BY objsubid;
SELECT pg_advisory_unlock_all();
SELECT count(*) FROM pg_locks WHERE locktype = 'advisory';

-- One backend may own both modes, including mixed session/xact scope.
BEGIN;
SELECT pg_advisory_lock_shared(-1, 2);
SELECT pg_advisory_xact_lock(-1, 2);
SELECT classid, objid, objsubid, mode, granted
  FROM pg_locks
 WHERE locktype = 'advisory'
 ORDER BY mode;
SAVEPOINT advisory_savepoint;
SELECT pg_advisory_xact_lock(7);
SELECT count(*) FROM pg_locks
 WHERE locktype = 'advisory' AND classid = 0 AND objid = 7 AND objsubid = 1;
ROLLBACK TO advisory_savepoint;
SELECT count(*) FROM pg_locks
 WHERE locktype = 'advisory' AND classid = 0 AND objid = 7 AND objsubid = 1;
ROLLBACK;

-- The session hold survives rollback; the xact hold does not.
SELECT classid, objid, objsubid, mode
  FROM pg_locks
 WHERE locktype = 'advisory';
SELECT pg_advisory_unlock_shared(-1, 2);
SELECT pg_advisory_unlock_shared(-1, 2);

SELECT oid, proname, prorettype, proargtypes::text, provolatile,
       proparallel, proisstrict, prosrc
  FROM pg_proc
 WHERE oid IN (2026,2561,2880,2881,2882,2883,2884,2885,2886,2887,
               2888,2889,2890,2891,2892,3089,3090,3091,3092,3093,
               3094,3095,3096)
 ORDER BY oid;

SELECT c.oid, c.reltype, c.relkind, a.attnum, a.attname, a.atttypid
  FROM pg_class c
  JOIN pg_attribute a ON a.attrelid = c.oid
 WHERE c.relname = 'pg_locks' AND a.attnum > 0
 ORDER BY a.attnum;
