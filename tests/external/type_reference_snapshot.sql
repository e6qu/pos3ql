SELECT n.nspname, t.typname, obj_description(t.oid, 'pg_type')
FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname IN ('type_scope', 'type_after') AND t.typname IN
('shared_domain', 'renamed_domain', 'shared_enum', 'renamed_enum',
 'shared_record', 'renamed_record') ORDER BY n.nspname, t.typname;
SELECT a.attname, n.nspname, t.typname
FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE a.attrelid = 'type_scope.items'::regclass
  AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum;
SELECT id::text, states::text, payload::text FROM type_scope.items;
SELECT pg_get_function_arguments(p.oid), pg_get_function_result(p.oid)
FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = 'type_scope' AND p.proname = 'echo_domain';
