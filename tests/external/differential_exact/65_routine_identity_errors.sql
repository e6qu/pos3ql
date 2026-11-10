-- Routine identities in DROP, ALTER, COMMENT and GRANT name the signature as
-- PostgreSQL's func_signature_string does, and report the wrong routine kind,
-- an ambiguous bare name, a missing schema and a missing argument type in
-- PostgreSQL's order. IF EXISTS skip notices are pinned by in-process tests.
DROP FUNCTION identity_missing(integer);
DROP FUNCTION identity_missing(varchar(3), timestamptz, int[], numeric(10,2), "char", char(5));
DROP FUNCTION identity_missing(int[3], integer array, national character varying, setof text);
DROP FUNCTION identity_missing;
DROP FUNCTION identity_missing();
DROP PROCEDURE identity_missing(int);
DROP PROCEDURE identity_missing;
DROP ROUTINE identity_missing(int);
DROP AGGREGATE identity_missing(int ORDER BY text);
DROP AGGREGATE identity_missing(*);
DROP FUNCTION pg_catalog.identity_missing(int);
DROP FUNCTION identity_nosch.identity_missing(int);
DROP FUNCTION identity_nosch.identity_missing(identity_notype);
DROP FUNCTION identity_missing(identity_nosch.t);
DROP FUNCTION identity_missing(anyelement, anyarray);
ALTER FUNCTION identity_missing(int) OWNER TO postgres;
ALTER AGGREGATE identity_missing(*) RENAME TO identity_other;
COMMENT ON FUNCTION identity_missing(int) IS 'x';
GRANT EXECUTE ON PROCEDURE identity_missing(int) TO public;
CREATE SCHEMA identity_hidden;
CREATE TYPE identity_hidden.pair AS (a int);
DROP FUNCTION identity_missing(identity_hidden.pair, identity_hidden.pair[]);
CREATE FUNCTION identity_fn(int) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION identity_fn(text) RETURNS int LANGUAGE sql AS 'SELECT 2';
CREATE PROCEDURE identity_proc(int) LANGUAGE sql AS 'SELECT 1';
CREATE AGGREGATE identity_agg(int) (sfunc = int4pl, stype = int);
DROP FUNCTION IF EXISTS identity_proc(int);
DROP PROCEDURE identity_fn(int);
DROP AGGREGATE identity_fn(int);
DROP FUNCTION IF EXISTS identity_agg;
ALTER FUNCTION identity_proc(int) OWNER TO postgres;
GRANT EXECUTE ON FUNCTION identity_proc(int) TO public;
DROP FUNCTION IF EXISTS identity_fn;
DROP ROUTINE identity_fn;
DROP PROCEDURE identity_fn;
DROP FUNCTION identity_fn(int), identity_missing(int);
ALTER AGGREGATE identity_agg(ORDER BY int) RENAME TO identity_agg2;
DROP AGGREGATE identity_agg2(ORDER BY int);
DROP FUNCTION identity_fn(int), identity_fn(text), identity_fn(int);
DROP PROCEDURE identity_proc;
DROP SCHEMA identity_hidden CASCADE;

-- Arrays of types without comparison operators fail when compared.
SELECT ARRAY['a']::refcursor[] = ARRAY['a']::refcursor[];
SELECT ARRAY['a']::refcursor[] <> ARRAY['a']::refcursor[];
SELECT ARRAY['a']::refcursor[] = ARRAY['a','b']::refcursor[];
SELECT ARRAY['a']::refcursor[] < ARRAY['a','b']::refcursor[];
SELECT ARRAY['{}']::json[] = ARRAY['{}']::json[];
SELECT '{}'::json[] >= '{}'::json[];
SELECT '{}'::cid[] < '{}'::cid[];
SELECT NULLIF(ARRAY['a']::refcursor[], ARRAY['b']::refcursor[]);
SELECT ARRAY['a']::refcursor[] @> '{}'::refcursor[];
SELECT array_position('{a}'::refcursor[], 'a');
SELECT array_remove('{a}'::refcursor[], NULL);
