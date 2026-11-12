-- A cast to a type that does not exist fails with 42704 wherever the cast
-- is typed, including inside pg_typeof.
SELECT pg_typeof('x'::no_such_type);
SELECT pg_typeof('x'::no_such_type[]);
SELECT 'x'::no_such_type;
