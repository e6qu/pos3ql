-- A set-returning SQL function in FROM that PostgreSQL inlines becomes a
-- subquery of its body also with a window, a column coerced to the declared
-- type, a polymorphic or variadic argument.
CREATE FUNCTION sw894(integer) RETURNS TABLE (g integer, r bigint) LANGUAGE sql IMMUTABLE
  AS $$SELECT g, row_number() OVER (ORDER BY g) FROM generate_series(1, $1) g$$;
CREATE FUNCTION sc894(integer) RETURNS TABLE (g integer) LANGUAGE sql IMMUTABLE
  AS $$SELECT g::bigint FROM generate_series(1, $1) g$$;
CREATE FUNCTION sl894(integer) RETURNS SETOF integer LANGUAGE sql IMMUTABLE
  AS $$SELECT g FROM generate_series(1, $1) g$$;
CREATE FUNCTION sp894(anyelement) RETURNS SETOF anyelement LANGUAGE sql IMMUTABLE
  AS $$SELECT $1$$;
CREATE FUNCTION sv894(VARIADIC integer[]) RETURNS SETOF integer LANGUAGE sql IMMUTABLE
  AS $$SELECT unnest($1)$$;
CREATE TABLE t894 (a integer, b integer);
INSERT INTO t894 SELECT g, g % 4 FROM generate_series(1, 20) g;
ANALYZE t894;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM sw894(5);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM sc894(5);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM sp894('x'::text);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM sp894(1::bigint) WHERE sp894 > 0;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM sv894(1, 2, 3);
SELECT * FROM sw894(3);
