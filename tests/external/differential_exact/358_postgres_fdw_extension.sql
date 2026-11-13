-- postgres_fdw is an extension in PostgreSQL's $system directory: its
-- handler and validator exist only once CREATE EXTENSION has run.
SELECT count(*) FROM pg_proc WHERE proname LIKE 'postgres_fdw%';
CREATE FOREIGN DATA WRAPPER fdw358 HANDLER postgres_fdw_handler;
CREATE EXTENSION postgres_fdw VERSION '1.0';
SELECT p.proname, p.prolang::regproc IS NULL, l.lanname, p.prosrc, p.probin,
       p.pronamespace::regnamespace, pg_get_function_identity_arguments(p.oid)
  FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang
 WHERE p.proname LIKE 'postgres_fdw%' ORDER BY p.proname;
SELECT pg_get_functiondef('postgres_fdw_validator'::regproc);
ALTER EXTENSION postgres_fdw UPDATE;
SELECT p.proname, p.prosrc, pg_get_function_arguments(p.oid), p.proretset
  FROM pg_proc p WHERE p.proname LIKE 'postgres_fdw%' ORDER BY p.proname;
SELECT extname, extversion, extrelocatable FROM pg_extension WHERE extname = 'postgres_fdw';
SELECT fdwname, fdwhandler::regproc, fdwvalidator::regproc, fdwoptions
  FROM pg_foreign_data_wrapper ORDER BY fdwname;
SELECT foreign_data_wrapper_name, foreign_data_wrapper_language
  FROM information_schema.foreign_data_wrappers ORDER BY 1;
SELECT pg_describe_object(classid, objid, objsubid), deptype FROM pg_depend
 WHERE refclassid = 'pg_extension'::regclass
   AND refobjid = (SELECT oid FROM pg_extension WHERE extname = 'postgres_fdw')
 ORDER BY 1;
CREATE FOREIGN DATA WRAPPER fdw358 HANDLER postgres_fdw_handler VALIDATOR postgres_fdw_validator;
CREATE FOREIGN DATA WRAPPER fdw358_bad HANDLER postgres_fdw_validator;
CREATE FOREIGN DATA WRAPPER fdw358_bad VALIDATOR postgres_fdw_handler;
CREATE SERVER srv358 FOREIGN DATA WRAPPER fdw358 OPTIONS (host '127.0.0.1', nope 'x');
CREATE SERVER srv358 FOREIGN DATA WRAPPER fdw358 OPTIONS (host '127.0.0.1');
SELECT pg_describe_object(classid, objid, objsubid), deptype FROM pg_depend
 WHERE classid = 'pg_foreign_data_wrapper'::regclass AND refclassid = 'pg_proc'::regclass
 ORDER BY 1;
DROP FUNCTION postgres_fdw_handler();
DROP EXTENSION postgres_fdw;
DROP EXTENSION postgres_fdw CASCADE;
SELECT count(*) FROM pg_proc WHERE proname LIKE 'postgres_fdw%';
SELECT count(*) FROM pg_foreign_data_wrapper;
-- A C routine binds to a routine of a module pos3ql implements.
CREATE FUNCTION handler358() RETURNS fdw_handler
  AS '$libdir/postgres_fdw', 'postgres_fdw_handler' LANGUAGE C;
CREATE FUNCTION handler358_sql() RETURNS integer AS 'select 1', 'x' LANGUAGE sql;
CREATE FOREIGN DATA WRAPPER fdw358_own HANDLER handler358;
SELECT fdwhandler::regproc FROM pg_foreign_data_wrapper WHERE fdwname = 'fdw358_own';
DROP FUNCTION handler358();
DROP FUNCTION handler358() CASCADE;
SELECT fdwname FROM pg_foreign_data_wrapper;
