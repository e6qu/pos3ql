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
