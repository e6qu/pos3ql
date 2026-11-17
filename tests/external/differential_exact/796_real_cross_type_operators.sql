-- `real` against an integer or `numeric` operand resolves to the
-- `real`-`double precision` operator (`float48mul`, `float48lt`, ...):
-- both it and the `real` operator need one implicit cast, and
-- `func_select_candidate` prefers the numeric category's preferred type, so
-- the other operand becomes `double precision`, as does the result.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/parser/parse_func.c
CREATE TABLE t796 (r real, s smallint, i int, b bigint, n numeric, d float8);
INSERT INTO t796 VALUES (1.5, 2, 3, 4, 5.5, 6.5), (2.25, -1, 0, 7, 0.5, -2);
SELECT pg_typeof(r * 2), r * 2, pg_typeof(r + 2.5), r + 2.5, pg_typeof(r * s), pg_typeof(r * i), pg_typeof(r * b), pg_typeof(r * n), pg_typeof(r / 3), pg_typeof(r - d), pg_typeof(r * r) FROM t796 ORDER BY r;
SELECT pg_typeof(2 * r), pg_typeof(s * r), pg_typeof(n - r), r * 0.1, r / 3 FROM t796 ORDER BY r;
EXPLAIN (VERBOSE, COSTS OFF) SELECT r * 2, r + 2.5, r * s, 2 * r, r < 2, r = 1.5, s * 2, r * i, n - r, r * d, r = '1.5' FROM t796 WHERE r > 1 AND r * 2 > 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT (r * 2) * 3, (r * s) + i, r IS DISTINCT FROM 2 FROM t796;
CREATE VIEW v796 AS SELECT r * 2 AS twice, r < 2 AS small, r + n AS mixed FROM t796;
SELECT pg_get_viewdef('v796'::regclass);
SELECT * FROM v796 ORDER BY twice;
