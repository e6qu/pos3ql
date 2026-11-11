-- Without a query ORDER BY, window query rows leave in the order of the
-- last window PostgreSQL evaluates, ties in the earlier windows' order;
-- a query ORDER BY sorts stably from there.
CREATE TABLE w189 (id int, g int, v int);
INSERT INTO w189 VALUES (1, 2, 30), (2, 1, 10), (3, 2, 20), (4, 1, 40), (5, 3, 5);
SELECT id, rank() OVER (ORDER BY v) FROM w189;
SELECT id, sum(v) OVER (PARTITION BY g) FROM w189;
SELECT id, sum(v) OVER (PARTITION BY g ORDER BY v) FROM w189;
SELECT id, row_number() OVER (ORDER BY v DESC) FROM w189;
SELECT id, sum(v) OVER (PARTITION BY g), rank() OVER (ORDER BY v) FROM w189;
SELECT id, rank() OVER (ORDER BY v), sum(v) OVER (PARTITION BY g) FROM w189;
SELECT id, count(*) OVER () FROM w189;
SELECT id, sum(v) OVER w FROM w189 WINDOW w AS (PARTITION BY g ORDER BY id DESC);
SELECT id, rank() OVER (ORDER BY v) FROM w189 WHERE id > 1;
SELECT id, rank() OVER (ORDER BY v) FROM w189 LIMIT 2;
SELECT id, g, rank() OVER (ORDER BY v DESC) FROM w189 ORDER BY g;
SELECT id, sum(v) OVER (PARTITION BY g ORDER BY v DESC NULLS LAST) FROM w189;
SELECT g, v, first_value(id) OVER (PARTITION BY v % 2 ORDER BY g) FROM w189;
SELECT * FROM (SELECT id, rank() OVER (ORDER BY v DESC) FROM w189) s;
SELECT id, sum(v) OVER (PARTITION BY g), rank() OVER w FROM w189 WINDOW w AS (ORDER BY v);
SELECT id, rank() OVER w, sum(v) OVER (PARTITION BY g) FROM w189 WINDOW w AS (ORDER BY v);
SELECT id, sum(v) OVER (w ORDER BY v DESC) FROM w189 WINDOW w AS (PARTITION BY g);
