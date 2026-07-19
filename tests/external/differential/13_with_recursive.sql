-- classic counter
WITH RECURSIVE t(n) AS (
  SELECT 1
  UNION ALL
  SELECT n + 1 FROM t WHERE n < 5
) SELECT * FROM t;
-- sum over the result
WITH RECURSIVE t(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 100
) SELECT sum(n) FROM t;
-- UNION (dedup) termination on a cycle
WITH RECURSIVE t(n) AS (
  SELECT 1 UNION SELECT (n % 3) + 1 FROM t
) SELECT * FROM t ORDER BY n;
-- string building
WITH RECURSIVE t(s) AS (
  SELECT 'a'::text UNION ALL SELECT s || 'a' FROM t WHERE length(s) < 4
) SELECT * FROM t ORDER BY s;
-- graph walk over a table
CREATE TABLE edges (src int, dst int);
INSERT INTO edges VALUES (1,2),(2,3),(3,4),(2,5),(5,6);
WITH RECURSIVE reach(node) AS (
  SELECT 2
  UNION
  SELECT e.dst FROM edges e JOIN reach r ON e.src = r.node
) SELECT * FROM reach ORDER BY node;
-- multiple columns, arithmetic
WITH RECURSIVE fib(a, b) AS (
  SELECT 0::bigint, 1::bigint
  UNION ALL
  SELECT b, a + b FROM fib WHERE b < 100
) SELECT a FROM fib ORDER BY a;
-- recursive CTE joined against itself downstream
WITH RECURSIVE t(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 4
) SELECT x.n, y.n FROM t x JOIN t y ON y.n = x.n + 1 ORDER BY x.n;
-- non-recursive CTE alongside a recursive one
WITH RECURSIVE base(v) AS (SELECT 10),
t(n) AS (
  SELECT v FROM base UNION ALL SELECT n - 1 FROM t WHERE n > 7
) SELECT * FROM t ORDER BY n;
-- RECURSIVE keyword without self-reference (plain CTE)
WITH RECURSIVE t(n) AS (SELECT 42) SELECT * FROM t;
-- column list renames
WITH RECURSIVE cnt(x) AS (
  SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 3
) SELECT x, x * 10 FROM cnt ORDER BY x;
-- shape errors
WITH RECURSIVE t(n) AS (SELECT n + 1 FROM t) SELECT * FROM t;
WITH RECURSIVE t(n) AS (
  SELECT n FROM t UNION ALL SELECT 1
) SELECT * FROM t;
DROP TABLE edges;
