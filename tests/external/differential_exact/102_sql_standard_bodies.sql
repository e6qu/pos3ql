-- pg_get_function_sqlbody returns a SQL-standard body in PostgreSQL's
-- analyzed form: RETURN expressions and BEGIN ATOMIC SELECT lists.
CREATE FUNCTION sb_return(x integer) RETURNS integer LANGUAGE sql RETURN x * 2;
CREATE FUNCTION sb_select(x integer) RETURNS integer LANGUAGE sql
  BEGIN ATOMIC SELECT x + 1; END;
CREATE FUNCTION sb_pair(x integer, y text) RETURNS text LANGUAGE sql
  BEGIN ATOMIC SELECT x * 2, y || 'z'; SELECT y; END;
CREATE FUNCTION sb_string(x integer) RETURNS integer LANGUAGE sql AS 'SELECT x';
SELECT pg_get_function_sqlbody('sb_return(integer)'::regprocedure);
SELECT pg_get_function_sqlbody('sb_select(integer)'::regprocedure);
SELECT pg_get_function_sqlbody('sb_pair(integer, text)'::regprocedure);
SELECT pg_get_function_sqlbody('sb_string(integer)'::regprocedure) IS NULL;
SELECT pg_get_functiondef('sb_select(integer)'::regprocedure);
SELECT pg_get_functiondef('sb_pair(integer, text)'::regprocedure);
SELECT sb_return(2), sb_select(2), sb_pair(2, 'y');
DROP FUNCTION sb_return(integer), sb_select(integer), sb_pair(integer, text),
  sb_string(integer);
