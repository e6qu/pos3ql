-- A view whose query CREATE OR REPLACE RULE "_RETURN" wrote prints its
-- columns qualified, as PostgreSQL's rule query also holds OLD and NEW; an
-- option change keeps that, and CREATE OR REPLACE VIEW drops it.
CREATE TABLE t361 (a int, b text);
CREATE VIEW v361 AS SELECT a, b FROM t361;
SELECT pg_get_viewdef('v361');
CREATE OR REPLACE RULE "_RETURN" AS ON SELECT TO v361
  DO INSTEAD SELECT a, b FROM t361 WHERE a > 0;
SELECT pg_get_viewdef('v361'), pg_get_viewdef('v361', true);
SELECT pg_get_ruledef(oid) FROM pg_rewrite WHERE ev_class = 'v361'::regclass;
ALTER VIEW v361 SET (security_barrier = true);
SELECT pg_get_viewdef('v361');
CREATE OR REPLACE VIEW v361 AS SELECT a, b FROM t361;
SELECT pg_get_viewdef('v361');
DROP VIEW v361;
DROP TABLE t361;
