-- Dropping a type that routine signatures use names the type and lists the
-- routines, or reports them as dropped with CASCADE.
CREATE TYPE tdr_pair AS (a int);
CREATE FUNCTION tdr_arg(tdr_pair) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION tdr_result(x int) RETURNS tdr_pair LANGUAGE sql AS 'SELECT ROW(1)::tdr_pair';
DROP TYPE tdr_pair;
DROP TYPE tdr_pair CASCADE;
CREATE TYPE tdr_mood AS ENUM ('a');
CREATE FUNCTION tdr_mood_arg(tdr_mood) RETURNS int LANGUAGE sql AS 'SELECT 1';
DROP TYPE tdr_mood;
DROP TYPE tdr_mood CASCADE;
CREATE DOMAIN tdr_count AS int;
CREATE FUNCTION tdr_count_arg(tdr_count) RETURNS int LANGUAGE sql AS 'SELECT 1';
DROP DOMAIN tdr_count;
DROP DOMAIN tdr_count CASCADE;
