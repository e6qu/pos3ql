-- Built-in routines carry PostgreSQL's shipped pg_description text; COMMENT
-- replaces it and COMMENT ... IS NULL removes it in the current database.
SELECT obj_description('lower(text)'::regprocedure, 'pg_proc'), obj_description('lower(text)'::regprocedure), obj_description('upper(text)'::regprocedure, 'pg_class');
SELECT description FROM pg_description WHERE objoid = 'md5(text)'::regprocedure AND classoid = 'pg_proc'::regclass;
COMMENT ON FUNCTION lower(text) IS 'my lower';
SELECT obj_description('lower(text)'::regprocedure, 'pg_proc');
COMMENT ON FUNCTION lower(text) IS NULL;
SELECT obj_description('lower(text)'::regprocedure, 'pg_proc'), (SELECT count(*) FROM pg_description WHERE objoid = 'lower(text)'::regprocedure);
COMMENT ON FUNCTION upper(text) IS '';
SELECT obj_description('upper(text)'::regprocedure, 'pg_proc');
SELECT count(*) > 1500 FROM pg_description d JOIN pg_proc p ON p.oid = d.objoid AND d.classoid = 'pg_proc'::regclass;
SELECT p.oid::regprocedure, d.description FROM pg_proc p JOIN pg_description d ON d.objoid = p.oid AND d.classoid = 'pg_proc'::regclass WHERE p.proname IN ('abs', 'now', 'generate_series', 'array_agg') ORDER BY p.oid;
COMMENT ON FUNCTION upper(text) IS 'converts string to upper case';
COMMENT ON FUNCTION lower(text) IS 'lowercase';
