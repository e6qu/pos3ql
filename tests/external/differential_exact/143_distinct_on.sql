-- DISTINCT ON keeps the first row of each run of equal expressions after
-- sorting by ORDER BY, then by DISTINCT ON expressions missing from it.
-- PostgreSQL accepts the expressions in any order among the leading
-- ORDER BY items, and reports a mismatch or a bad position at the
-- DISTINCT ON expression.
CREATE TABLE distinct_on_rows(a int, b int, c text);
INSERT INTO distinct_on_rows VALUES (1, 2, 'x'), (1, 1, 'y'), (2, 1, 'z'), (NULL, 3, 'n'),
  (NULL, 1, 'm'), (2, 2, 'w'), (1, 1, 'v');
SELECT DISTINCT ON (a, b) a, b, c FROM distinct_on_rows ORDER BY b, a, c;
SELECT DISTINCT ON (a, b) a, b, c FROM distinct_on_rows ORDER BY a, b, c;
SELECT DISTINCT ON (a, b) a, b FROM distinct_on_rows ORDER BY a;
SELECT DISTINCT ON (a) a, b, c FROM distinct_on_rows ORDER BY a, b DESC, c;
SELECT DISTINCT ON (a) a, b, c FROM distinct_on_rows ORDER BY a NULLS FIRST, c;
SELECT DISTINCT ON (1) a, b FROM distinct_on_rows ORDER BY 1 DESC, b;
SELECT DISTINCT ON (b) b, a FROM distinct_on_rows ORDER BY b DESC, a LIMIT 2 OFFSET 1;
SELECT DISTINCT ON (d.a) d.a, v.tag FROM distinct_on_rows d
  JOIN (VALUES (1, 'p'), (2, 'q')) v(k, tag) ON v.k = d.a ORDER BY d.a, v.tag;
SELECT DISTINCT ON (g % 3) g % 3 AS r, g FROM generate_series(1, 30) g ORDER BY g % 3, g DESC;
SELECT DISTINCT ON (a) a, b, c FROM distinct_on_rows ORDER BY b;
SELECT DISTINCT ON (a, b) a, b, c FROM distinct_on_rows ORDER BY a, c;
SELECT DISTINCT ON (a, b) a, b, c FROM distinct_on_rows ORDER BY c, a;
SELECT DISTINCT ON (5) a, b FROM distinct_on_rows;
SELECT a FROM distinct_on_rows ORDER BY 7;
