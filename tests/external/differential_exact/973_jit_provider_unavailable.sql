-- pos3ql loads no JIT provider, as a PostgreSQL built without LLVM: jit
-- is on and jit_provider names llvmjit, a postmaster setting, yet
-- pg_jit_available() is false and EXPLAIN of a plan above jit_above_cost
-- prints no JIT section, since no function was compiled. The developer
-- options are kept and shown: the superuser-backend ones are set only at
-- connection start, jit_dump_bitcode only by a superuser.
SHOW jit;
SHOW jit_provider;
SELECT pg_jit_available();
SELECT name, setting, unit, category, short_desc, extra_desc, context, vartype,
       source, min_val, max_val, enumvals, boot_val, reset_val, sourcefile,
       sourceline, pending_restart
  FROM pg_settings
 WHERE name LIKE 'jit%' ORDER BY name;
SELECT name, pg_settings_get_flags(name) FROM pg_settings
 WHERE name LIKE 'jit%' ORDER BY name;
SELECT pg_settings_get_flags('JIT_DUMP_BITCODE'), pg_settings_get_flags('jit.x');
SET jit_provider = 'llvmjit';
RESET jit_provider;
SELECT current_setting('jit_provider');
SHOW jit_debugging_support;
SET jit_debugging_support = on;
RESET jit_profiling_support;
SELECT set_config('jit_profiling_support', 'on', false);
SET jit_dump_bitcode = on;
SHOW jit_dump_bitcode;
SET jit_expressions = off;
SHOW jit_expressions;
SET jit_tuple_deforming = 'maybe';
BEGIN;
SET LOCAL jit_tuple_deforming = off;
SHOW jit_tuple_deforming;
ROLLBACK;
SHOW jit_tuple_deforming;
SELECT name, setting, source FROM pg_settings
 WHERE name IN ('jit_dump_bitcode', 'jit_expressions') ORDER BY name;
SET default_statistics_target = 50;
SELECT pg_settings_get_flags('default_statistics_target');
EXPLAIN (SETTINGS, COSTS OFF) SELECT 1;
RESET default_statistics_target;
RESET jit_dump_bitcode;
RESET jit_expressions;
CREATE ROLE jit_user LOGIN;
SET ROLE jit_user;
SHOW jit_dump_bitcode;
SET jit_dump_bitcode = on;
SET jit_debugging_support = on;
SET jit_expressions = off;
SHOW jit_expressions;
RESET jit_expressions;
RESET ROLE;
ALTER ROLE jit_user SET jit_profiling_support = on;
ALTER ROLE jit_user SET jit_dump_bitcode = on;
SELECT setconfig FROM pg_db_role_setting
 WHERE setrole = 'jit_user'::regrole;
DROP ROLE jit_user;
CREATE TABLE jt (id int, v int);
INSERT INTO jt SELECT g, g % 10 FROM generate_series(1, 2000) g;
ANALYZE jt;
SET jit_above_cost = 0;
SET jit_inline_above_cost = 0;
SET jit_optimize_above_cost = 0;
EXPLAIN SELECT sum(a.v * b.v) FROM jt a JOIN jt b ON a.v = b.v WHERE a.id < 1500;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT sum(a.v * b.v) FROM jt a JOIN jt b ON a.v = b.v WHERE a.id < 1500;
RESET jit_above_cost;
RESET jit_inline_above_cost;
RESET jit_optimize_above_cost;
DROP TABLE jt;
