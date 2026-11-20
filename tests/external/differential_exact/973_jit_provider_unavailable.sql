-- pos3ql loads no JIT provider, as a PostgreSQL built without LLVM: jit
-- is on and jit_provider names llvmjit, a postmaster setting, yet
-- pg_jit_available() is false and EXPLAIN of a plan above jit_above_cost
-- prints no JIT section, since no function was compiled.
SHOW jit;
SHOW jit_provider;
SELECT pg_jit_available();
SELECT name, setting, unit, category, short_desc, extra_desc, context, vartype,
       source, min_val, max_val, enumvals, boot_val, reset_val, sourcefile,
       sourceline, pending_restart
  FROM pg_settings
 WHERE name IN ('jit', 'jit_above_cost', 'jit_inline_above_cost',
                'jit_optimize_above_cost', 'jit_provider')
 ORDER BY name;
SET jit_provider = 'llvmjit';
RESET jit_provider;
SELECT current_setting('jit_provider');
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
