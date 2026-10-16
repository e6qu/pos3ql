SELECT 1 WHERE FALSE LIMIT -1;
SELECT 1 WHERE FALSE OFFSET -1;
SELECT pg_typeof(1)::oid, pg_typeof(1)::integer, pg_typeof(1)::bigint;
SELECT pg_typeof(1)::smallint;
SELECT NULL::regtype::smallint;
SELECT 23::oid::smallint;
SELECT 'pg_class'::regclass::smallint;
PREPARE rejected_type_cast AS SELECT NULL::regtype::smallint;
CREATE SCHEMA routine_domains;
CREATE DOMAIN routine_domains.positive AS integer NOT NULL CHECK (VALUE > 0);
CREATE DOMAIN routine_domains.other AS integer CHECK (VALUE > 0);
CREATE DOMAIN routine_domains.wide AS bigint CHECK (VALUE > 0);
CREATE FUNCTION routine_domains.echo(arg routine_domains.positive) RETURNS integer
LANGUAGE SQL AS 'SELECT arg::integer';
CREATE FUNCTION routine_domains.widen(arg routine_domains.wide) RETURNS bigint
LANGUAGE SQL AS 'SELECT arg::bigint';
CREATE FUNCTION routine_domains.first(arg routine_domains.positive[]) RETURNS integer
LANGUAGE SQL AS 'SELECT arg[1]::integer';
CREATE FUNCTION routine_domains.choose(arg integer) RETURNS text
LANGUAGE SQL AS 'SELECT ''base''::text';
CREATE FUNCTION routine_domains.choose(arg routine_domains.positive) RETURNS text
LANGUAGE SQL AS 'SELECT ''domain''::text';

SELECT routine_domains.echo(7), routine_domains.echo(7::smallint),
       routine_domains.echo(7::routine_domains.other),
       routine_domains.echo(arg => 7), routine_domains.widen(7);
SELECT routine_domains.choose(7),
       routine_domains.choose(7::routine_domains.positive);
SELECT routine_domains.first(ARRAY[7, 8]);
SELECT routine_domains.echo(-1);
SELECT routine_domains.echo(NULL);
SELECT routine_domains.echo(7::bigint);
SELECT routine_domains.first(ARRAY[7, -8]);
CREATE SEQUENCE routine_domains.cast_calls START 7;
CREATE FUNCTION routine_domains.small_type(arg regtype) RETURNS smallint
LANGUAGE SQL VOLATILE STRICT AS 'SELECT nextval(''routine_domains.cast_calls'')::smallint';
CREATE CAST (regtype AS smallint) WITH FUNCTION routine_domains.small_type(regtype);
SELECT pg_typeof(1)::smallint, (NULL::regtype::smallint) IS NULL;
SELECT pg_typeof(1)::smallint = 8 AND TRUE;
SELECT FALSE AND pg_typeof(1)::smallint > 0;
SELECT currval('routine_domains.cast_calls');
CREATE FUNCTION routine_domains.bump() RETURNS bigint
LANGUAGE SQL VOLATILE AS 'SELECT nextval(''routine_domains.cast_calls'')';
CREATE FUNCTION routine_domains.nested() RETURNS bigint
LANGUAGE SQL VOLATILE AS 'SELECT nextval(''routine_domains.cast_calls'') + routine_domains.bump()';
SELECT routine_domains.nested();
SELECT currval('routine_domains.cast_calls');
DROP FUNCTION routine_domains.nested();
DROP FUNCTION routine_domains.bump();
SELECT nextval('routine_domains.cast_calls') WHERE FALSE;
SELECT generate_series(1, 3) WHERE nextval('routine_domains.cast_calls') > 0;
SELECT currval('routine_domains.cast_calls');
SELECT generate_series(1, nextval('routine_domains.cast_calls')::integer) WHERE FALSE;
SELECT currval('routine_domains.cast_calls');
SELECT nextval('routine_domains.cast_calls') LIMIT 0;
SELECT currval('routine_domains.cast_calls');
CREATE TABLE routine_domains.filtered(value integer);
INSERT INTO routine_domains.filtered SELECT generate_series(1, 3)
WHERE nextval('routine_domains.cast_calls') > 0;
SELECT value FROM routine_domains.filtered ORDER BY value;
SELECT currval('routine_domains.cast_calls');
DROP CAST (regtype AS smallint);
CREATE OR REPLACE FUNCTION routine_domains.small_type(arg regtype) RETURNS smallint
LANGUAGE SQL IMMUTABLE STRICT AS 'SELECT (1 / 0)::smallint';
CREATE CAST (regtype AS smallint) WITH FUNCTION routine_domains.small_type(regtype);
SELECT pg_typeof(1)::smallint WHERE FALSE;
CREATE FUNCTION routine_domains.small_type(arg boolean) RETURNS smallint
LANGUAGE SQL IMMUTABLE STRICT AS 'SELECT (1 / 0)::smallint';
CREATE CAST (boolean AS smallint) WITH FUNCTION routine_domains.small_type(boolean);
SELECT TRUE::smallint WHERE FALSE;
DROP CAST (boolean AS smallint);
DROP CAST (regtype AS smallint);
DROP FUNCTION routine_domains.small_type(regtype);
DROP FUNCTION routine_domains.small_type(boolean);
CREATE FUNCTION routine_domains.equal_positive(a routine_domains.positive, b routine_domains.positive)
RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN TRUE;
CREATE FUNCTION routine_domains.equal_other(a routine_domains.other, b routine_domains.other)
RETURNS boolean LANGUAGE SQL IMMUTABLE RETURN FALSE;
CREATE OPERATOR routine_domains.=== (
    FUNCTION = routine_domains.equal_positive,
    LEFTARG = routine_domains.positive, RIGHTARG = routine_domains.positive);
CREATE OPERATOR routine_domains.=== (
    FUNCTION = routine_domains.equal_other,
    LEFTARG = routine_domains.other, RIGHTARG = routine_domains.other);
SELECT 7::routine_domains.positive OPERATOR(routine_domains.===) 8::routine_domains.positive,
       7::routine_domains.other OPERATOR(routine_domains.===) 8::routine_domains.other;
PREPARE exact_domain_operator AS
SELECT $1::routine_domains.positive OPERATOR(routine_domains.===) $2::routine_domains.positive;
EXECUTE exact_domain_operator(7, 8);
DEALLOCATE exact_domain_operator;
SELECT 7 OPERATOR(routine_domains.===) 8;
DROP SCHEMA routine_domains CASCADE;
