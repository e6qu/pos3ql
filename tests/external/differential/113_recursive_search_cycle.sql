-- PostgreSQL 18 recursive SEARCH/CYCLE rewriting: generated values, types,
-- name resolution, cycle termination, and stored-query boundaries.

WITH RECURSIVE walk(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM walk WHERE n < 4
) SEARCH DEPTH FIRST BY n SET ord
SELECT n, ord, pg_typeof(ord) FROM walk ORDER BY ord;

WITH RECURSIVE walk(n, label) AS (
  SELECT 1, 'a'::text
  UNION ALL
  SELECT n + 1, label || 'x' FROM walk WHERE n < 3
) SEARCH BREADTH FIRST BY n, label SET ord
  CYCLE n, label SET cyc TO 'Y' DEFAULT 'N' USING path
SELECT n, label, ord, cyc, path, pg_typeof(ord), pg_typeof(cyc), pg_typeof(path)
FROM walk ORDER BY ord;

WITH RECURSIVE cycle(n) AS (
  SELECT 1
  UNION ALL
  SELECT CASE WHEN n = 3 THEN 1 ELSE n + 1 END FROM cycle
) CYCLE n SET is_cycle USING path
SELECT n, is_cycle, path FROM cycle;

-- Generated columns are visible to explicit recursive-term references, but
-- remain absent from the recursive term's star expansion.
WITH RECURSIVE walk(n) AS (
  SELECT 1
  UNION ALL
  SELECT n + 1 FROM walk WHERE n < 3 AND walk.ord IS NOT NULL
) SEARCH DEPTH FIRST BY n SET ord
SELECT n, ord FROM walk ORDER BY n;

WITH RECURSIVE quoted("N") AS (
  SELECT 1 UNION ALL SELECT "N" + 1 FROM quoted WHERE "N" < 2
) SEARCH DEPTH FIRST BY "N" SET "Path"
SELECT "N", "Path" FROM quoted;

CREATE VIEW durable_cycle_view AS
WITH RECURSIVE cycle(n) AS (
  SELECT 1
  UNION ALL
  SELECT CASE WHEN n = 3 THEN 1 ELSE n + 1 END FROM cycle
) SEARCH DEPTH FIRST BY n SET ord CYCLE n SET is_cycle USING path
SELECT n, is_cycle FROM cycle;
SELECT * FROM durable_cycle_view;
DROP VIEW durable_cycle_view;

-- Anonymous record outputs are query-local pseudo-types and cannot become
-- stored relation columns.
CREATE VIEW invalid_search_view AS
WITH RECURSIVE walk(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM walk WHERE n < 2
) SEARCH DEPTH FIRST BY n SET ord
SELECT n, ord FROM walk;

CREATE TABLE invalid_cycle_table AS
WITH RECURSIVE walk(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM walk WHERE n < 2
) CYCLE n SET is_cycle USING path
SELECT n, path FROM walk;

-- Clause and generated-name validation.
WITH walk(n) AS (SELECT 1)
SEARCH DEPTH FIRST BY n SET ord SELECT * FROM walk;
WITH RECURSIVE walk(n) AS (SELECT 1)
CYCLE n SET is_cycle USING path SELECT * FROM walk;
WITH RECURSIVE walk(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM walk WHERE n < 2
) SEARCH DEPTH FIRST BY missing SET ord SELECT * FROM walk;
WITH RECURSIVE walk(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM walk WHERE n < 2
) CYCLE n SET n USING path SELECT * FROM walk;
