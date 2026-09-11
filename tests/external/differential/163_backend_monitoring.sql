-- PostgreSQL 18 backend monitoring and administration surfaces.
SELECT string_agg(a.attname || ':' || a.atttypid::text, ',' ORDER BY a.attnum)
  FROM pg_attribute a
  JOIN pg_class c ON c.oid = a.attrelid
 WHERE c.relname = 'pg_stat_activity' AND a.attnum > 0 AND NOT a.attisdropped;

SELECT datname = current_database(), pid = pg_backend_pid(), usesysid IS NOT NULL,
       usename = current_user, state = 'active', backend_type = 'client backend',
       backend_start IS NOT NULL, xact_start IS NOT NULL,
       query LIKE 'SELECT datname = current_database()%'
  FROM pg_stat_activity
 WHERE pid = pg_backend_pid();

SELECT ssl, version IS NULL, cipher IS NULL, bits IS NULL
  FROM pg_stat_ssl WHERE pid = pg_backend_pid();

SELECT pg_cancel_backend(2147483647), pg_terminate_backend(2147483647),
       pg_notification_queue_usage() BETWEEN 0.0 AND 1.0;

LISTEN backend_monitoring_channel;
SELECT * FROM pg_listening_channels();
UNLISTEN *;
SELECT count(*) FROM pg_listening_channels();

SELECT count(*), bool_and(confl_tablespace = 0 AND confl_lock = 0
                         AND confl_snapshot = 0 AND confl_bufferpin = 0
                         AND confl_deadlock = 0 AND confl_active_logicalslot = 0)
  FROM pg_stat_database_conflicts WHERE datname = current_database();

SELECT oid, proname, pronargs, prorettype, provolatile, proparallel, proretset, prosrc
  FROM pg_proc
 WHERE oid IN (2096, 2171, 3035, 3296)
 ORDER BY oid;
