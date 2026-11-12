-- An untyped literal passed as anyelement with an anyarray takes the array's
-- element type, as PostgreSQL resolves polymorphic arguments.
SELECT array_replace('{a}'::refcursor[], 'a', 'b');
SELECT array_append('{a}'::refcursor[], 'b'), array_prepend('z', '{a}'::refcursor[]);
SELECT array_append('{1}'::int[], '2'), array_append(NULL::int[], '2');
SELECT array_append('{1}'::int[], 'x');
SELECT array_prepend('1.5', '{1}'::int[]);
SELECT pg_typeof(array_append('{a}'::name[], 'b')), pg_typeof(array_prepend('b', '{a}'::varchar[]));
SELECT array_position('{a}'::refcursor[], 'a'), array_remove('{a,b}'::refcursor[], 'a');
