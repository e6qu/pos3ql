-- A row constructor casts to a table's row type field by field, and
-- `= ANY`/`= ALL` over arrays of records or named composites compare
-- element fields with record_eq.
CREATE TYPE pt187 AS (x int, y numeric);
SELECT (1, 2)::pt187 = ANY (ARRAY[(1, 2.0)::pt187]);
SELECT (1, 2)::pt187 = ALL (ARRAY[(1, 2.0)::pt187, NULL]);
SELECT ROW(1, 2.0) = ANY (ARRAY[ROW(1, 2.00), ROW(3, 4)]);
CREATE TABLE t187(a int, b text);
INSERT INTO t187 VALUES (1, 'a'), (2, 'b');
SELECT ROW(1, 'a')::t187;
SELECT (ROW(1, 'a')::t187).b;
SELECT ROW(1, 'a')::t187 = t187 FROM t187 ORDER BY a;
SELECT ROW(1.5, 'a')::t187, NULL::t187 IS NULL;
SELECT ROW(1)::t187;
SELECT ROW(1, 'a', 3)::t187;
