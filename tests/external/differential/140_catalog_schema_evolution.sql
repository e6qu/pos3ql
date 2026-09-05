-- One catalog identity must survive connected DDL transitions. The final
-- reader query crosses type, table, view, routine, aggregate, index, owner,
-- schema, and ACL boundaries rather than treating them as independent names.

CREATE ROLE catalog_evolution_owner;
CREATE ROLE catalog_evolution_reader;
CREATE SCHEMA catalog_evolution;
GRANT USAGE, CREATE ON SCHEMA catalog_evolution
  TO catalog_evolution_owner, catalog_evolution_reader;

CREATE TYPE catalog_evolution.state AS ENUM ('new', 'ready');
CREATE TABLE catalog_evolution.rows (
  id integer PRIMARY KEY,
  state catalog_evolution.state NOT NULL,
  amount integer NOT NULL
);
INSERT INTO catalog_evolution.rows VALUES (1, 'new', 2), (2, 'ready', 5);
CREATE INDEX catalog_evolution_rows_state_idx
  ON catalog_evolution.rows (state, id);
CREATE FUNCTION catalog_evolution.bump(integer) RETURNS integer
  LANGUAGE SQL IMMUTABLE RETURN $1 + 1;
CREATE FUNCTION catalog_evolution.sum_state(bigint, integer) RETURNS bigint
  LANGUAGE SQL IMMUTABLE RETURN coalesce($1, 0) + $2;
CREATE AGGREGATE catalog_evolution.sum_amount(integer) (
  SFUNC = catalog_evolution.sum_state,
  STYPE = bigint
);
CREATE VIEW catalog_evolution.rows_view AS
  SELECT id, state, catalog_evolution.bump(amount) AS bumped
    FROM catalog_evolution.rows;

ALTER TYPE catalog_evolution.state RENAME TO row_state;
ALTER TABLE catalog_evolution.rows RENAME TO rows_moved;
ALTER INDEX catalog_evolution.catalog_evolution_rows_state_idx
  RENAME TO rows_moved_state_idx;
ALTER FUNCTION catalog_evolution.bump(integer) RENAME TO bump_moved;
ALTER FUNCTION catalog_evolution.bump_moved(integer) SET SCHEMA public;
ALTER AGGREGATE catalog_evolution.sum_amount(integer) RENAME TO sum_amount_moved;
ALTER AGGREGATE catalog_evolution.sum_amount_moved(integer) SET SCHEMA public;
ALTER VIEW catalog_evolution.rows_view RENAME TO rows_view_moved;
ALTER VIEW catalog_evolution.rows_view_moved SET SCHEMA public;
ALTER TABLE catalog_evolution.rows_moved OWNER TO catalog_evolution_owner;
ALTER TYPE catalog_evolution.row_state OWNER TO catalog_evolution_owner;
ALTER FUNCTION public.bump_moved(integer) OWNER TO catalog_evolution_owner;
ALTER AGGREGATE public.sum_amount_moved(integer) OWNER TO catalog_evolution_owner;
ALTER VIEW public.rows_view_moved OWNER TO catalog_evolution_owner;

GRANT USAGE ON TYPE catalog_evolution.row_state TO catalog_evolution_reader;
GRANT SELECT ON TABLE catalog_evolution.rows_moved TO catalog_evolution_reader;
GRANT SELECT ON TABLE public.rows_view_moved TO catalog_evolution_reader;
GRANT EXECUTE ON FUNCTION public.bump_moved(integer) TO catalog_evolution_reader;
REVOKE EXECUTE ON FUNCTION public.sum_amount_moved(integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.sum_amount_moved(integer) TO catalog_evolution_reader;

SELECT public.bump_moved(amount), public.sum_amount_moved(amount)
  FROM catalog_evolution.rows_moved;
SELECT state, bumped FROM public.rows_view_moved ORDER BY id;
SELECT indexname FROM pg_indexes
 WHERE schemaname = 'catalog_evolution' AND tablename = 'rows_moved';
SELECT namespace.nspname, pg_get_userbyid(procedure.proowner)
  FROM pg_proc procedure
  JOIN pg_namespace namespace ON namespace.oid = procedure.pronamespace
 WHERE procedure.proname IN ('bump_moved', 'sum_amount_moved')
 ORDER BY procedure.proname;

SET ROLE catalog_evolution_reader;
SELECT state, bumped FROM public.rows_view_moved ORDER BY id;
SELECT public.bump_moved(amount)
  FROM catalog_evolution.rows_moved ORDER BY id;
RESET ROLE;

DROP VIEW public.rows_view_moved;
DROP AGGREGATE public.sum_amount_moved(integer);
DROP FUNCTION public.bump_moved(integer);
DROP FUNCTION catalog_evolution.sum_state(bigint, integer);
DROP TABLE catalog_evolution.rows_moved;
DROP TYPE catalog_evolution.row_state;
REVOKE USAGE, CREATE ON SCHEMA catalog_evolution
  FROM catalog_evolution_owner, catalog_evolution_reader;
DROP SCHEMA catalog_evolution;
DROP ROLE catalog_evolution_reader;
DROP ROLE catalog_evolution_owner;
