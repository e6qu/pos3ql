SELECT oid, proname, pronamespace, prorettype, proowner, prolang, prosrc
  FROM pg_proc
 WHERE oid IN (89, 2079, 2081)
 ORDER BY oid;

SELECT pg_function_is_visible(89),
       pg_function_is_visible(999999),
       pg_table_is_visible('pg_proc'::regclass),
       pg_type_is_visible(26),
       pg_collation_is_visible(100),
       pg_relation_is_publishable('pg_proc'::regclass);
