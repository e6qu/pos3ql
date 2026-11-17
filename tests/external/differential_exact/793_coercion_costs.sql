-- A condition's cost charges the cast function that coerces a built-in
-- function's argument to the type it declares (`length(name)` reads
-- `text(name)`), and none where the types are binary coercible or a
-- cross-type operator compares them (`int2 = int4`, `real < double
-- precision`), as `cost_qual_eval` costs the planned expression.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/costsize.c
CREATE TABLE t793 (n name, v varchar(20), c char(8), i int2, t text, b int8, r real, d float8, m numeric);
INSERT INTO t793 SELECT ('n' || g)::name, 'v' || g, 'c' || (g % 9), (g % 300)::int2, 't' || g, g * 3, g / 2.0, g / 3.0, g / 7.0 FROM generate_series(1, 1500) g;
ANALYZE t793;
EXPLAIN SELECT * FROM t793 WHERE length(n) > 3;
EXPLAIN SELECT * FROM t793 WHERE length(v) > 3;
EXPLAIN SELECT * FROM t793 WHERE length(c) > 3;
EXPLAIN SELECT * FROM t793 WHERE upper(n) = 'N5';
EXPLAIN SELECT * FROM t793 WHERE abs(i) > 3 AND length(n) > 3;
EXPLAIN SELECT * FROM t793 WHERE length(t) > 3 AND length(n) > 3;
EXPLAIN SELECT * FROM t793 WHERE sqrt(i) > 3;
EXPLAIN SELECT * FROM t793 WHERE length(n) > 3 AND i = 7;
EXPLAIN SELECT * FROM t793 WHERE b + i > 100;
EXPLAIN SELECT * FROM t793 WHERE substr(n, 1, 2) = 'n1';
EXPLAIN SELECT * FROM t793 WHERE i * 2 < 10::int8;
