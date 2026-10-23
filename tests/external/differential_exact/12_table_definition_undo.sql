-- An order keeps its values through column changes and a rolled-back rename.
CREATE TABLE table_edit_orders (id integer PRIMARY KEY, note text);
INSERT INTO table_edit_orders VALUES (1, 'order');
BEGIN;
ALTER TABLE table_edit_orders RENAME COLUMN note TO description;
ALTER TABLE table_edit_orders ADD COLUMN amount integer DEFAULT 7;
SAVEPOINT before_temporary_shape;
ALTER TABLE table_edit_orders RENAME TO table_edit_orders_work;
ALTER TABLE table_edit_orders_work DROP COLUMN description;
ROLLBACK TO before_temporary_shape;
RELEASE before_temporary_shape;
ALTER TABLE table_edit_orders ALTER COLUMN id TYPE bigint;
COMMIT;
SELECT id, description, amount FROM table_edit_orders ORDER BY id;

-- Replacing a private table, then rolling back, restores the original table.
BEGIN;
CREATE TABLE table_edit_private (id integer PRIMARY KEY, note text);
INSERT INTO table_edit_private VALUES (11, 'original');
ALTER TABLE table_edit_private RENAME TO table_edit_selected;
SAVEPOINT before_replacement;
DROP TABLE table_edit_selected;
CREATE TABLE table_edit_selected (id bigint PRIMARY KEY, note text);
INSERT INTO table_edit_selected VALUES (22, 'replacement');
ALTER TABLE table_edit_selected ADD COLUMN marker text DEFAULT 'new';
ROLLBACK TO before_replacement;
RELEASE before_replacement;
SELECT id, note FROM table_edit_selected ORDER BY id;
COMMIT;
SELECT id, note FROM table_edit_selected ORDER BY id;

-- A type rename and a table edit publish together without losing either.
CREATE TYPE table_edit_state AS ENUM ('ready', 'done');
CREATE TABLE table_edit_typed (id integer, state table_edit_state);
INSERT INTO table_edit_typed VALUES (1, 'ready');
BEGIN;
ALTER TABLE table_edit_typed ADD COLUMN note text DEFAULT 'kept';
ALTER TYPE table_edit_state RENAME TO table_edit_state_renamed;
COMMIT;
SELECT id, state, note FROM table_edit_typed ORDER BY id;
