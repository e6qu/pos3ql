SELECT pg_typeof(id)::text, pg_typeof(states)::text, pg_typeof(payload)::text
FROM type_scope.items;
SELECT obj_description(pg_typeof(id)::oid, 'pg_type'),
       obj_description(pg_typeof(states[1])::oid, 'pg_type'),
       obj_description(pg_typeof(payload)::oid, 'pg_type')
FROM type_scope.items;
SELECT id::text, states::text, payload::text FROM type_scope.items;
SELECT pg_get_function_arguments(p.oid), pg_get_function_result(p.oid)
FROM pg_proc p WHERE p.pronamespace = 'type_scope'::regnamespace
  AND p.proname = 'echo_domain';
