-- LATERAL joins, matching PostgreSQL 18. A LATERAL FROM item may reference the
-- columns of the FROM items to its left and is re-evaluated per outer row:
-- projected outer expressions, correlated subqueries (CROSS/LEFT JOIN LATERAL),
-- aggregates, and set-returning functions taking an outer argument.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS lat_t;
DROP TABLE IF EXISTS lat_u;

CREATE TABLE lat_t (id int, n int);
INSERT INTO lat_t VALUES (1, 2), (2, 3), (3, 0);
CREATE TABLE lat_u (tid int, v text);
INSERT INTO lat_u VALUES (1, 'a'), (1, 'b'), (2, 'c');

-- A FROM-less lateral body projects an outer expression, re-evaluated per row.
SELECT id, d FROM lat_t, LATERAL (SELECT lat_t.n * 2 AS d) s ORDER BY id;
SELECT id, x FROM lat_t, LATERAL (SELECT lat_t.id + lat_t.n AS x) s ORDER BY id;

-- CROSS JOIN LATERAL over a correlated subquery: the inner scan sees the outer
-- row (the correlation is in the inner WHERE). Rows with no match drop out.
SELECT t.id, s.v
  FROM lat_t t CROSS JOIN LATERAL (SELECT v FROM lat_u WHERE lat_u.tid = t.id) s
  ORDER BY t.id, s.v;

-- Comma is a cross join too.
SELECT t.id, s.v
  FROM lat_t t, LATERAL (SELECT v FROM lat_u WHERE lat_u.tid = t.id) s
  ORDER BY t.id, s.v;

-- LEFT JOIN LATERAL keeps a left row that the lateral side produces nothing for,
-- nulling the lateral columns (id 3 has no matching lat_u row).
SELECT t.id, s.v
  FROM lat_t t LEFT JOIN LATERAL (SELECT v FROM lat_u WHERE lat_u.tid = t.id) s ON true
  ORDER BY t.id, s.v;

-- An aggregate inside the lateral body: one row per outer row, count included
-- for the outer rows with no match (0), which the LEFT JOIN preserves.
SELECT t.id, s.c
  FROM lat_t t, LATERAL (SELECT count(*) AS c FROM lat_u WHERE lat_u.tid = t.id) s
  ORDER BY t.id;

-- A set-returning function taking an outer column as its argument. An outer row
-- whose series is empty (n = 0) contributes no rows.
SELECT id, g FROM lat_t, LATERAL generate_series(1, lat_t.n) g ORDER BY id, g;

-- Two lateral items in a row, the second referencing the first's output.
SELECT t.id, a.g, b.doubled
  FROM lat_t t,
       LATERAL generate_series(1, t.n) a(g),
       LATERAL (SELECT a.g * 10 AS doubled) b
  ORDER BY t.id, a.g;

-- A non-lateral derived table still works beside a lateral one, unchanged.
SELECT t.id, s.v
  FROM lat_t t
  JOIN LATERAL (SELECT v FROM lat_u WHERE lat_u.tid = t.id) s ON true
  ORDER BY t.id, s.v;

-- Cleanup.
DROP TABLE lat_t;
DROP TABLE lat_u;
