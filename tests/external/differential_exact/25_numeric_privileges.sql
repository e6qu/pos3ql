CREATE ROLE numeric_privilege_reader;
SELECT has_type_privilege(2147483647::oid, 'USAGE')
   AND has_type_privilege(2147483648::oid, 'USAGE')
   AND has_type_privilege(4294967295::oid, 'USAGE')
   AND has_type_privilege(2147483647::oid, 'USAGE WITH GRANT OPTION')
   AND has_schema_privilege(2147483647::oid, 'USAGE, CREATE WITH GRANT OPTION')
   AND has_function_privilege(2147483647::oid, 'EXECUTE WITH GRANT OPTION')
   AND has_language_privilege(2147483647::oid, 'USAGE WITH GRANT OPTION')
   AND has_database_privilege(2147483647::oid, 'CONNECT WITH GRANT OPTION')
   AND has_tablespace_privilege(2147483647::oid, 'CREATE WITH GRANT OPTION')
   AND has_foreign_data_wrapper_privilege(2147483647::oid, 'USAGE WITH GRANT OPTION')
   AND has_server_privilege(2147483647::oid, 'USAGE WITH GRANT OPTION');
SELECT has_type_privilege('numeric_privilege_reader', 2147483647::oid, 'USAGE') IS NULL
   AND has_schema_privilege('numeric_privilege_reader', 2147483647::oid, 'USAGE') IS NULL
   AND has_function_privilege('numeric_privilege_reader', 2147483647::oid, 'EXECUTE') IS NULL
   AND has_language_privilege('numeric_privilege_reader', 2147483647::oid, 'USAGE') IS NULL
   AND has_database_privilege('numeric_privilege_reader', 2147483647::oid, 'CONNECT') IS NULL
   AND has_tablespace_privilege('numeric_privilege_reader', 2147483647::oid, 'CREATE') IS NULL
   AND has_tablespace_privilege('numeric_privilege_reader', 211663::oid, 'CREATE') IS NULL
   AND has_tablespace_privilege('numeric_privilege_reader', 211664::oid, 'CREATE') IS NULL
   AND has_foreign_data_wrapper_privilege('numeric_privilege_reader', 2147483647::oid, 'USAGE') IS NULL
   AND has_server_privilege('numeric_privilege_reader', 2147483647::oid, 'USAGE') IS NULL
   AND has_largeobject_privilege('numeric_privilege_reader', 2147483647::oid, 'SELECT') IS NULL;
SELECT has_table_privilege(2147483647::oid, 'SELECT') IS NULL
   AND has_sequence_privilege(2147483647::oid, 'USAGE') IS NULL
   AND has_largeobject_privilege(2147483647::oid, 'SELECT WITH GRANT OPTION') IS NULL
   AND pg_type_is_visible(4294967295::oid) IS NULL
   AND format_type(4294967295::oid, NULL::integer) = '???'
   AND format_type('4294967295', NULL::integer) = '???';
SELECT lo_create(4294967295::oid) = 4294967295::oid;
GRANT SELECT ON LARGE OBJECT 4294967295 TO numeric_privilege_reader;
SELECT has_largeobject_privilege('numeric_privilege_reader', 4294967295::oid, 'SELECT')
   AND has_largeobject_privilege(4294967295::oid, 'UPDATE WITH GRANT OPTION');
SELECT lo_unlink(4294967295::oid) = 1;
DROP ROLE numeric_privilege_reader;
