-- STRICT and SECURITY of a built-in change only its pg_proc row: PostgreSQL
-- runs built-ins with their compiled null handling and as the caller, and its
-- planner folds a constant NULL input of a function marked strict to NULL.
CREATE ROLE abr_owner;
CREATE ROLE abr_caller;
CREATE TABLE abr_t(a integer);
GRANT SELECT ON abr_t TO abr_owner;
CREATE TABLE abr_n(a integer);
INSERT INTO abr_n VALUES (NULL);
SELECT array_append(ARRAY[1], NULL);
ALTER FUNCTION array_append(anycompatiblearray, anycompatible) STRICT;
SELECT proisstrict FROM pg_proc WHERE oid = 'array_append(anycompatiblearray, anycompatible)'::regprocedure;
SELECT array_append(ARRAY[1], NULL), array_append(ARRAY[1], NULL::integer), array_append(ARRAY[1], 2);
SELECT array_append(ARRAY[1], a) FROM abr_n;
ALTER FUNCTION array_append(anycompatiblearray, anycompatible) CALLED ON NULL INPUT;
SELECT array_append(ARRAY[1], NULL);
ALTER FUNCTION lower(text) CALLED ON NULL INPUT;
SELECT lower(NULL) IS NULL;
ALTER FUNCTION lower(text) STRICT;
DROP TABLE abr_n;
ALTER FUNCTION has_table_privilege(text, text) SECURITY DEFINER;
ALTER FUNCTION has_table_privilege(text, text) OWNER TO abr_owner;
SELECT prosecdef, proowner::regrole FROM pg_proc WHERE oid = 'has_table_privilege(text, text)'::regprocedure;
SET ROLE abr_caller;
SELECT has_table_privilege('abr_t', 'select'), current_user;
RESET ROLE;
-- OWNER TO is its own ALTER FUNCTION form; restore the built-in for later fixtures.
ALTER FUNCTION has_table_privilege(text, text) SECURITY INVOKER;
ALTER FUNCTION has_table_privilege(text, text) OWNER TO postgres;
SET ROLE abr_caller;
SELECT has_table_privilege('abr_t', 'select');
RESET ROLE;
SET ROLE abr_caller;
ALTER FUNCTION lower(text) STRICT;
ALTER FUNCTION lower(text) SECURITY DEFINER;
RESET ROLE;
SELECT proisstrict, prosecdef, proconfig, proname, pronamespace::regnamespace FROM pg_proc WHERE oid IN ('lower(text)'::regprocedure, 'array_append(anycompatiblearray, anycompatible)'::regprocedure) ORDER BY oid;
DROP TABLE abr_t;
DROP ROLE abr_owner;
DROP ROLE abr_caller;
