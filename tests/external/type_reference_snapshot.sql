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

SELECT OPERATOR(type_scope.##) id, states[1]::varchar FROM type_scope.items;
SELECT oprname, oprright::regtype::text, oprresult::regtype::text
FROM pg_operator WHERE oprnamespace = 'type_scope'::regnamespace
AND oprname IN ('##', '===') ORDER BY oprname;
SELECT opcname, opcintype::regtype::text, opckeytype = 0
FROM pg_opclass WHERE opcnamespace = 'type_scope'::regnamespace AND opcname = 'domain_ops';
SELECT amoplefttype::regtype::text, amoprighttype::regtype::text
FROM pg_amop WHERE amopfamily = (
    SELECT oid FROM pg_opfamily WHERE opfnamespace = 'type_scope'::regnamespace AND opfname = 'domain_ops');
SELECT amproclefttype::regtype::text, amprocrighttype::regtype::text
FROM pg_amproc WHERE amprocfamily = (
    SELECT oid FROM pg_opfamily WHERE opfnamespace = 'type_scope'::regnamespace AND opfname = 'domain_ops');
SELECT castsource::regtype::text, casttarget::regtype::text FROM pg_cast
WHERE castsource = (SELECT pg_typeof(states[1])::oid FROM type_scope.items)
AND casttarget = 'varchar'::regtype;
