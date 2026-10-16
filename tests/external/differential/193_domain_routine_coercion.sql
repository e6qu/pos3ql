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
CREATE FUNCTION routine_domains.small_type(arg regtype) RETURNS smallint
LANGUAGE SQL STRICT AS 'SELECT 7::smallint';
CREATE CAST (regtype AS smallint) WITH FUNCTION routine_domains.small_type(regtype);
SELECT pg_typeof(1)::smallint, (NULL::regtype::smallint) IS NULL;
DROP CAST (regtype AS smallint);
DROP SCHEMA routine_domains CASCADE;
