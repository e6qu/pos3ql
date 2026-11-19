-- A window's input sort holds the columns the planner reads: a USING-merged
-- column is its join alias variable (the left column for inner and left
-- joins, the right for a right join, both for a full join), and (expr).*
-- reads the record's own columns. The joins here order their windows by unique
-- keys; the record expansion's ties follow the sort's spilled runs.
CREATE TABLE l921 (a int, b int);
CREATE TABLE r921 (a int, c int);
INSERT INTO l921 SELECT g % 40, g FROM generate_series(1, 450) g;
INSERT INTO r921 SELECT g, g FROM generate_series(1, 40) g;
SELECT a, b, row_number() OVER (ORDER BY b) FROM l921 JOIN r921 USING (a) ORDER BY 3 OFFSET 200 LIMIT 5;
SELECT a, b, rank() OVER (ORDER BY b) FROM l921 LEFT JOIN r921 USING (a) ORDER BY 3 OFFSET 200 LIMIT 5;
SELECT a, c, row_number() OVER (ORDER BY c) FROM l921 RIGHT JOIN r921 USING (a) ORDER BY 3 OFFSET 20 LIMIT 5;
SELECT a, c, row_number() OVER (ORDER BY c, b) FROM l921 FULL JOIN r921 USING (a) ORDER BY 3 OFFSET 200 LIMIT 5;
SELECT *, row_number() OVER (ORDER BY b) FROM l921 NATURAL JOIN r921 ORDER BY 4 LIMIT 5;
SET work_mem = '64kB';
SELECT (ROW(a, b)).*, row_number() OVER (ORDER BY b % 3) FROM l921 ORDER BY 3, 2 OFFSET 200 LIMIT 10;
SELECT a, row_number() OVER (ORDER BY a) FROM l921 ORDER BY 2 OFFSET 300 LIMIT 5;
