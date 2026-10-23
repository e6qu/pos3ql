-- Inserts, updates and deletes use the private layout selected by their statement.
CREATE TABLE row_definition_orders (id integer PRIMARY KEY, note text);
INSERT INTO row_definition_orders VALUES (1, 'first'), (2, 'second'), (3, 'third');
BEGIN;
ALTER TABLE row_definition_orders ADD COLUMN amount integer DEFAULT 5;
UPDATE row_definition_orders SET amount = 8 WHERE id = 1;
SAVEPOINT before_rewrite;
ALTER TABLE row_definition_orders ALTER COLUMN amount TYPE bigint;
UPDATE row_definition_orders SET note = 'discarded', amount = 90 WHERE id = 1;
DELETE FROM row_definition_orders WHERE id = 2;
INSERT INTO row_definition_orders VALUES (4, 'discarded', 40);
ROLLBACK TO before_rewrite;
RELEASE before_rewrite;
UPDATE row_definition_orders SET amount = 9 WHERE id = 3;
ALTER TABLE row_definition_orders ALTER COLUMN amount TYPE bigint;
COMMIT;
SELECT id, note, amount FROM row_definition_orders ORDER BY id;

-- Rolled-back replacement rows cannot change the restored table or its index.
BEGIN;
ALTER TABLE row_definition_orders RENAME COLUMN amount TO total;
UPDATE row_definition_orders SET total = 12 WHERE id = 2;
SAVEPOINT before_replacement;
DROP TABLE row_definition_orders;
CREATE TABLE row_definition_orders (id bigint PRIMARY KEY, note text, total bigint);
INSERT INTO row_definition_orders VALUES (99, 'replacement', 99);
ROLLBACK TO before_replacement;
RELEASE before_replacement;
DELETE FROM row_definition_orders WHERE id = 3;
INSERT INTO row_definition_orders VALUES (4, 'fourth', 14);
COMMIT;
SELECT id, note, total FROM row_definition_orders ORDER BY id;
