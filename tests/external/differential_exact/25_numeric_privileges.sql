CREATE ROLE numeric_privilege_reader;
SELECT has_type_privilege(2147483647::oid, 'USAGE')
   AND has_type_privilege(2147483647::oid, 'USAGE WITH GRANT OPTION')
   AND has_schema_privilege(2147483647::oid, 'USAGE, CREATE WITH GRANT OPTION')
   AND has_function_privilege(2147483647::oid, 'EXECUTE WITH GRANT OPTION');
SELECT has_type_privilege('numeric_privilege_reader', 2147483647::oid, 'USAGE') IS NULL
   AND has_schema_privilege('numeric_privilege_reader', 2147483647::oid, 'USAGE') IS NULL
   AND has_function_privilege('numeric_privilege_reader', 2147483647::oid, 'EXECUTE') IS NULL;
SELECT has_table_privilege(2147483647::oid, 'SELECT') IS NULL
   AND has_sequence_privilege(2147483647::oid, 'USAGE') IS NULL;
DROP ROLE numeric_privilege_reader;
