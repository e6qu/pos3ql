-- Row-lock clause validation and constraint order. Concurrent rechecks are
-- exercised by the engine's two-session tests (row_lock_conflicts) and
-- mvcc_diff.py; this fixture pins the single-session errors and values.
CREATE SCHEMA locked_rechecks;
SET search_path = locked_rechecks, public;
CREATE TABLE items(id integer PRIMARY KEY, qty integer CHECK (qty > 0), note text);
CREATE TABLE stock(id integer, qty integer);
INSERT INTO items VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 3, 'c');
INSERT INTO stock VALUES (1, 1), (2, 2), (4, 4);
SELECT * FROM items LEFT JOIN stock ON stock.id = items.id FOR UPDATE;
SELECT * FROM items LEFT JOIN stock ON stock.id = items.id FOR UPDATE OF stock;
SELECT items.id FROM items LEFT JOIN stock ON stock.id = items.id ORDER BY 1 FOR UPDATE OF items;
SELECT * FROM items RIGHT JOIN stock ON stock.id = items.id FOR NO KEY UPDATE OF items;
SELECT * FROM items FULL JOIN stock ON stock.id = items.id FOR KEY SHARE OF stock;
SELECT id, generate_series(1, 2) FROM items FOR SHARE;
SELECT id FROM items, LATERAL (SELECT stock.qty FROM stock WHERE stock.id = items.id) s ORDER BY id FOR UPDATE OF items;
-- A CHECK violation is reported before a duplicate key, as PostgreSQL's
-- ExecConstraints runs before the index insertion.
INSERT INTO items VALUES (1, -1, 'x');
UPDATE items SET id = 1, qty = -1 WHERE id = 2;
MERGE INTO items USING (VALUES (2)) AS s(id) ON items.id = s.id
  WHEN MATCHED THEN UPDATE SET id = 1, qty = -1;
INSERT INTO items VALUES (1, 5, 'x');
UPDATE items SET id = id, note = note || '!' WHERE id = 1 RETURNING *;
BEGIN;
UPDATE items SET qty = qty + 10 WHERE id = 2;
SELECT * FROM items WHERE qty > 10 FOR UPDATE;
SELECT i.id, i.qty, s.qty FROM items i JOIN stock s USING (id) ORDER BY i.id FOR UPDATE OF i;
COMMIT;
RESET search_path;
DROP SCHEMA locked_rechecks CASCADE;
