-- A call giving a variadic input elements of a domain or enum type inlines
-- with the array its elements pack into; a call with no element does not
-- exist.
CREATE DOMAIN pos893 AS integer CHECK (VALUE > 0);
CREATE TYPE mood893 AS ENUM ('sad', 'ok', 'happy');
CREATE FUNCTION z893(VARIADIC integer[]) RETURNS integer LANGUAGE sql IMMUTABLE
  AS $$SELECT coalesce(array_length($1, 1), 0)$$;
CREATE FUNCTION d893(VARIADIC pos893[]) RETURNS integer LANGUAGE sql IMMUTABLE
  AS $$SELECT $1[1]::integer$$;
CREATE FUNCTION e893(VARIADIC mood893[]) RETURNS integer LANGUAGE sql IMMUTABLE
  AS $$SELECT array_length($1, 1)$$;
CREATE TABLE t893 (a integer, m mood893, s text);
INSERT INTO t893 VALUES (1, 'ok', 'x'), (2, 'sad', 'y');
EXPLAIN (VERBOSE, COSTS OFF) SELECT z893();
EXPLAIN (VERBOSE, COSTS OFF) SELECT d893(a) FROM t893;
EXPLAIN (VERBOSE, COSTS OFF) SELECT d893(a, 3) FROM t893;
EXPLAIN (VERBOSE, COSTS OFF) SELECT e893(m, 'ok') FROM t893;
SELECT d893(1, 2), e893('sad', 'ok');
SELECT d893(0);
