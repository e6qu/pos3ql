-- Joins and sorts a result stream or cursor reads row by row match
-- PostgreSQL: join cursors evaluate output per FETCH, a scrollable join
-- cursor returns a revisited row's first values, and volatile output runs
-- after a sort, in output order.
CREATE TABLE retained_orders(id int, customer int, note text);
INSERT INTO retained_orders SELECT g, g % 4, 'n' || g FROM generate_series(1, 12) g;
CREATE TABLE retained_customers(id int, name text);
INSERT INTO retained_customers VALUES (0, 'zero'), (1, 'one'), (3, NULL);
CREATE SEQUENCE retained_calls;
CREATE FUNCTION retained_label(i int) RETURNS text LANGUAGE sql AS $$ SELECT 'r' || i $$;
SELECT o.id, c.name FROM retained_orders o JOIN retained_customers c ON c.id = o.customer
  ORDER BY o.id DESC;
SELECT o.id, c.name FROM retained_orders o LEFT JOIN retained_customers c
  ON c.id = o.customer AND c.name IS NOT NULL ORDER BY c.name NULLS FIRST, o.id;
SELECT o.id, retained_label(o.id), nextval('retained_calls') FROM retained_orders o
  WHERE o.id % 3 = 0 ORDER BY o.id DESC;
SELECT id FROM retained_orders ORDER BY customer, id DESC LIMIT 4 OFFSET 2;
SELECT * FROM (VALUES (1)) v RIGHT JOIN generate_series(1, 3) g ON v.column1 = g;
BEGIN;
DECLARE one_match NO SCROLL CURSOR FOR
  SELECT nextval('retained_calls'), o.id, c.name FROM retained_orders o
    JOIN retained_customers c ON c.id = o.customer + 0 WHERE o.id = 4;
SELECT is_scrollable FROM pg_cursors WHERE name = 'one_match';
SELECT last_value FROM retained_calls;
FETCH 1 one_match;
SELECT last_value FROM retained_calls;
DECLARE scrolled SCROLL CURSOR FOR
  SELECT nextval('retained_calls'), o.id FROM retained_orders o
    JOIN retained_customers c ON c.id = o.customer + 0 WHERE o.id = 4;
FETCH 1 scrolled;
FETCH NEXT scrolled;
FETCH PRIOR scrolled;
FETCH FIRST scrolled;
SELECT last_value FROM retained_calls;
COMMIT;
