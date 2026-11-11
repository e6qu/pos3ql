-- DROP INDEX of the index a constraint owns names the constraint that
-- requires it.
CREATE TABLE t (a int PRIMARY KEY, b int UNIQUE, c int, CONSTRAINT t_c_key UNIQUE (c));
DROP INDEX t_pkey;
DROP INDEX IF EXISTS t_b_key;
DROP INDEX public.t_c_key;
CREATE UNIQUE INDEX t_ci ON t (c);
ALTER TABLE t ADD CONSTRAINT t_ci UNIQUE USING INDEX t_ci;
DROP INDEX t_ci;
DROP TABLE t;
