-- Default-privilege rows are identified and described as PostgreSQL does.
CREATE ROLE ia_r;
ALTER DEFAULT PRIVILEGES FOR ROLE ia_r IN SCHEMA public GRANT USAGE ON SEQUENCES TO public;
ALTER DEFAULT PRIVILEGES FOR ROLE ia_r GRANT EXECUTE ON FUNCTIONS TO postgres;
ALTER DEFAULT PRIVILEGES FOR ROLE ia_r GRANT USAGE ON TYPES TO postgres;
ALTER DEFAULT PRIVILEGES FOR ROLE ia_r GRANT USAGE ON SCHEMAS TO postgres;
SELECT (pg_identify_object('pg_default_acl'::regclass, oid, 0)).*,
       (pg_identify_object_as_address('pg_default_acl'::regclass, oid, 0)).*,
       pg_describe_object('pg_default_acl'::regclass, oid, 0)
  FROM pg_default_acl ORDER BY 4;
SELECT pg_describe_object('pg_default_acl'::regclass, 1, 0) IS NULL;
DROP OWNED BY ia_r;
DROP ROLE ia_r;
