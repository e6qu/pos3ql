-- Sorting jsonb, array and record values charges their PostgreSQL sizes
-- (varlena headers, container entries, array and tuple headers, alignment),
-- so the sort spills where PostgreSQL's does and ties follow its merge; jsonb
-- orders as compareJsonbContainers does (numbers by value, objects by size).
CREATE TABLE t918 (a int, j jsonb, r int[]);
INSERT INTO t918 SELECT g % 11, jsonb_build_object('k', g % 7, 'v', repeat('z', g % 9), 'n', g / 3.0), array[g % 5, g % 3, g] FROM generate_series(1, 1500) g;
SET work_mem = '64kB';
SELECT a, j FROM t918 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, r FROM t918 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, row(j, r) FROM t918 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, t918 FROM t918 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, j, r FROM t918 ORDER BY j OFFSET 700 LIMIT 20;
SELECT a, r FROM t918 ORDER BY r OFFSET 700 LIMIT 20;
SET work_mem = '256kB';
SELECT a, j FROM t918 ORDER BY a OFFSET 700 LIMIT 20;
SELECT a, row(a, j) FROM t918 ORDER BY a OFFSET 700 LIMIT 20;
SELECT j FROM (SELECT unnest(ARRAY['null','"a"','"aA"','"b"','1','22.0','218.0','true','false','[]','[1]','[1, 2]','[2]','[null]','[{}]','{}','{"a": 1}','{"a": 2}','{"aa": 1}','{"a": 1, "b": 1}','{"b": [1]}']::jsonb[]) j) x ORDER BY j;
