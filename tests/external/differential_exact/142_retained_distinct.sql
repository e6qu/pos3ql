-- Plain DISTINCT read by a retained reader (a result stream or paged
-- portal) groups by its output list: rows come out sorted by it, NULLs
-- last, and each distinct value keeps its first row's spelling, as
-- PostgreSQL's hashed DISTINCT does. The general executor agrees.
CREATE TABLE distinct_rows(id int, k int, label text, amount numeric, ratio float8);
INSERT INTO distinct_rows SELECT g, g % 4, 'l' || (g % 3), (g % 5)::numeric,
  CASE WHEN g % 2 = 0 THEN 0.0 ELSE g % 3 END FROM generate_series(1, 60) g;
INSERT INTO distinct_rows VALUES (61, NULL, NULL, 1.00, '-0'), (62, 2, 'l1', 2.0, NULL),
  (63, NULL, 'l0', NULL, 'NaN');
SELECT DISTINCT k, label FROM distinct_rows ORDER BY k, label;
SELECT DISTINCT amount FROM distinct_rows WHERE id > 60 OR id < 3 ORDER BY amount;
SELECT DISTINCT k + 1 AS next, label FROM distinct_rows ORDER BY next, label LIMIT 4 OFFSET 2;
SELECT DISTINCT ratio FROM distinct_rows WHERE id IN (61, 2, 63, 3) ORDER BY 1;
SELECT DISTINCT k FROM distinct_rows ORDER BY k DESC;
SELECT DISTINCT d.k, v.tag FROM distinct_rows d JOIN (VALUES (1, 'a'), (2, 'b'))
  v(k, tag) ON v.k = d.k ORDER BY d.k;
SELECT DISTINCT g % 3 AS r FROM generate_series(1, 3000) g ORDER BY r;
SELECT DISTINCT label FROM distinct_rows ORDER BY id;
BEGIN;
DECLARE distinct_cursor CURSOR FOR SELECT DISTINCT k, label FROM distinct_rows ORDER BY k, label;
FETCH 3 FROM distinct_cursor;
FETCH ALL FROM distinct_cursor;
COMMIT;
