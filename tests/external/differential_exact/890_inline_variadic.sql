-- A call of a SQL function with a variadic input inlines its body with the
-- elements packed into an array, so the plan prints, costs and matches an
-- index on the body in the call's place.
CREATE FUNCTION v890(VARIADIC integer[]) RETURNS integer LANGUAGE sql IMMUTABLE
  AS $$SELECT $1[1] + coalesce($1[2], 0)$$;
CREATE FUNCTION w890(base integer, VARIADIC integer[]) RETURNS integer
  LANGUAGE sql IMMUTABLE AS $$SELECT base + coalesce($2[1], 0)$$;
CREATE TABLE t890 (a integer, b integer);
INSERT INTO t890 SELECT g, g % 7 FROM generate_series(1, 500) g;
CREATE INDEX t890_a ON t890 ((a + coalesce(b, 0)));
ANALYZE t890;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v890(1, 2, 3);
EXPLAIN (VERBOSE, COSTS OFF) SELECT v890(a, b) FROM t890;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v890(a::smallint, b) FROM t890 WHERE b = 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v890(VARIADIC ARRAY[a, b]) FROM t890;
EXPLAIN (VERBOSE, COSTS OFF) SELECT w890(a, b, 1) FROM t890;
EXPLAIN SELECT * FROM t890 WHERE v890(a, b) = 10;
SELECT v890(1, 2, 3), v890(VARIADIC ARRAY[4, 5]), w890(1, 2);
SELECT count(*) FROM t890 WHERE v890(a, b) = 10;
