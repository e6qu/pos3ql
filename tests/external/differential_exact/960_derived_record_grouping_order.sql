-- A grouping or DISTINCT inside a derived table may return its rows in any
-- order when the query reading it cannot tell: its ORDER BY names every
-- output column, or it counts the rows. A select list of (t).* or of
-- (ROW(...)::type).* groups by the columns and arguments those fields
-- select, as PostgreSQL expands and simplifies them before planning, so
-- the hash order of such a grouping is the plain columns' order.
CREATE TABLE d960 (a int, b int, c text);
INSERT INTO d960 SELECT i % 50, i % 7, 'v' || (i % 11) FROM generate_series(1, 2000) i;
ANALYZE d960;
CREATE TYPE p960 AS (x int, y text);
SELECT * FROM (SELECT DISTINCT sum(b) AS s FROM d960 GROUP BY a) t ORDER BY s;
SELECT s, n FROM (SELECT DISTINCT sum(b) AS s, count(*) AS n FROM d960 GROUP BY a) t
  ORDER BY n, s;
SELECT * FROM (SELECT DISTINCT a % 9 AS k, b FROM d960) t ORDER BY 2, 1;
SELECT t.* FROM (SELECT a, b FROM d960 GROUP BY a, b) t ORDER BY b, a LIMIT 12;
SELECT count(*) FROM (SELECT DISTINCT a, b FROM d960) t;
SELECT count(*), min(c), max(c) FROM (SELECT DISTINCT b, c FROM d960) t;
SELECT (d960).* FROM d960 GROUP BY 1, 2, 3 ORDER BY 1 LIMIT 15;
SELECT (d960).a, (d960).b FROM d960 GROUP BY 1, 2 ORDER BY 1, 2 LIMIT 15;
SELECT (d960).*, count(*) FROM d960 GROUP BY a, b, c;
SELECT DISTINCT (ROW(a, c)::p960).* FROM d960;
SELECT DISTINCT (ROW(a, c)::p960).*, b FROM d960 ORDER BY 3;
SELECT DISTINCT (ROW(a, b)).* FROM d960;
SELECT DISTINCT (d960).* FROM d960 WHERE a < 3;
DROP TABLE d960;
DROP TYPE p960;
