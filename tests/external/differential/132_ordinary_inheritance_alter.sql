-- Parent ALTER TABLE propagates every shared definition change through ordinary
-- inheritance, while ONLY retains PostgreSQL's parent-local attributes.
DROP TABLE IF EXISTS ordinary_inheritance_alter_parent CASCADE;

CREATE TABLE ordinary_inheritance_alter_parent (id integer, label text);
CREATE TABLE ordinary_inheritance_alter_child (extra text)
  INHERITS (ordinary_inheritance_alter_parent);
CREATE TABLE ordinary_inheritance_alter_grandchild (flag boolean)
  INHERITS (ordinary_inheritance_alter_child);
INSERT INTO ordinary_inheritance_alter_child (id, label, extra)
  VALUES (1, 'child', 'c');
INSERT INTO ordinary_inheritance_alter_grandchild (id, label, extra, flag)
  VALUES (2, 'grandchild', 'g', true);

ALTER TABLE ordinary_inheritance_alter_parent
  ADD COLUMN amount integer DEFAULT 7 NOT NULL;
ALTER TABLE ordinary_inheritance_alter_parent
  ALTER COLUMN label SET DEFAULT 'parent-default';
ALTER TABLE ordinary_inheritance_alter_parent RENAME COLUMN label TO title;
ALTER TABLE ordinary_inheritance_alter_parent ALTER COLUMN amount TYPE bigint;
ALTER TABLE ordinary_inheritance_alter_parent
  ADD CONSTRAINT ordinary_inheritance_alter_amount_check CHECK (amount > 0);

SELECT id, title, amount, extra
  FROM ONLY ordinary_inheritance_alter_child ORDER BY id;
SELECT conislocal, coninhcount
  FROM pg_constraint
 WHERE conrelid = 'ordinary_inheritance_alter_grandchild'::regclass
   AND conname = 'ordinary_inheritance_alter_amount_check';

ALTER TABLE ONLY ordinary_inheritance_alter_parent ALTER COLUMN amount SET DEFAULT 99;
INSERT INTO ordinary_inheritance_alter_child (id, extra) VALUES (3, 'defaulted');
ALTER TABLE ordinary_inheritance_alter_parent ALTER COLUMN amount DROP NOT NULL;
ALTER TABLE ordinary_inheritance_alter_parent
  DROP CONSTRAINT ordinary_inheritance_alter_amount_check;
ALTER TABLE ordinary_inheritance_alter_child
  NO INHERIT ordinary_inheritance_alter_parent;
ALTER TABLE ordinary_inheritance_alter_child
  INHERIT ordinary_inheritance_alter_parent;
ALTER TABLE ONLY ordinary_inheritance_alter_parent ALTER COLUMN amount SET NOT NULL;
INSERT INTO ordinary_inheritance_alter_child (id, amount, extra)
  VALUES (4, NULL, 'parent-local-null');
ALTER TABLE ordinary_inheritance_alter_parent
  ADD COLUMN propagated_tail integer DEFAULT 4,
  ADD CONSTRAINT ordinary_inheritance_alter_parent_id_unique UNIQUE (id);

SELECT id, title, amount, extra, propagated_tail
  FROM ONLY ordinary_inheritance_alter_child ORDER BY id, extra;
SELECT count(*)
  FROM pg_constraint
 WHERE conrelid = 'ordinary_inheritance_alter_child'::regclass
   AND conname = 'ordinary_inheritance_alter_parent_id_unique';
ALTER TABLE ordinary_inheritance_alter_parent DROP COLUMN title;
SELECT id, amount, extra, propagated_tail
  FROM ONLY ordinary_inheritance_alter_child ORDER BY id, extra;
ALTER TABLE ONLY ordinary_inheritance_alter_parent ADD COLUMN split integer;

SET client_min_messages = warning;
DROP TABLE ordinary_inheritance_alter_parent CASCADE;

-- DML uses names to project reordered child columns and checks child constraints.
CREATE TABLE inheritance_dml_parent (id integer PRIMARY KEY, amount integer);
CREATE TABLE inheritance_dml_child
  (extra text, amount integer, id integer NOT NULL UNIQUE,
   derived integer GENERATED ALWAYS AS (amount * 2) STORED, CHECK (amount < 100));
ALTER TABLE inheritance_dml_child INHERIT inheritance_dml_parent;
INSERT INTO inheritance_dml_child (extra, amount, id) VALUES ('retained', 10, 1);
CREATE FUNCTION inheritance_dml_touch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.extra = NEW.extra || ':changed'; RETURN NEW; END $$;
CREATE TRIGGER inheritance_dml_touch BEFORE UPDATE OF amount ON inheritance_dml_child
  FOR EACH ROW EXECUTE FUNCTION inheritance_dml_touch();
UPDATE inheritance_dml_parent SET amount = amount + 1 WHERE id = 1
  RETURNING id, amount;
SELECT extra, amount, id, derived FROM ONLY inheritance_dml_child;
UPDATE inheritance_dml_parent AS target SET amount = source.amount
  FROM (VALUES (1, 20)) AS source(id, amount) WHERE target.id = source.id
  RETURNING target.id, target.amount;
UPDATE inheritance_dml_parent SET amount = 100 WHERE id = 1;
SELECT extra, amount, id, derived FROM ONLY inheritance_dml_child;
CREATE TABLE inheritance_dml_ref
  (child_id integer REFERENCES inheritance_dml_child(id) ON UPDATE CASCADE ON DELETE CASCADE);
INSERT INTO inheritance_dml_ref VALUES (1);
UPDATE inheritance_dml_parent SET id = 2 WHERE id = 1 RETURNING id, amount;
SELECT child_id FROM inheritance_dml_ref;
DELETE FROM inheritance_dml_parent USING (VALUES (2)) AS source(id)
  WHERE inheritance_dml_parent.id = source.id RETURNING inheritance_dml_parent.id, amount;
SELECT count(*) FROM inheritance_dml_ref;
INSERT INTO inheritance_dml_child (extra, amount, id) VALUES ('second', 30, 3);
DELETE FROM inheritance_dml_parent WHERE id = 3 RETURNING id, amount;
DROP TABLE inheritance_dml_parent CASCADE;
DROP TABLE inheritance_dml_child CASCADE;
DROP TABLE inheritance_dml_ref;
DROP FUNCTION inheritance_dml_touch();

-- A new parent column follows existing child-only columns in stored rows.
CREATE TABLE inheritance_tail_parent (id integer PRIMARY KEY);
CREATE TABLE inheritance_tail_child (extra text) INHERITS (inheritance_tail_parent);
ALTER TABLE inheritance_tail_parent ADD COLUMN amount integer DEFAULT 7;
INSERT INTO inheritance_tail_child (id, extra) VALUES (1, 'kept'), (1, 'also-kept');
UPDATE inheritance_tail_parent SET amount = amount + 1 WHERE id = 1;
SELECT id, extra, amount FROM ONLY inheritance_tail_child ORDER BY extra;
DELETE FROM inheritance_tail_parent WHERE amount = 8 RETURNING id, amount;
DROP TABLE inheritance_tail_parent CASCADE;
DROP TABLE inheritance_tail_child;

-- Partition moves and root foreign keys use the selected leaf layout.
CREATE TABLE partition_dml_parent (id integer PRIMARY KEY, amount integer)
  PARTITION BY RANGE (id);
CREATE TABLE partition_dml_left (id integer NOT NULL, amount integer);
CREATE TABLE partition_dml_right (id integer NOT NULL, amount integer);
ALTER TABLE partition_dml_parent ATTACH PARTITION partition_dml_left FOR VALUES FROM (0) TO (10);
ALTER TABLE partition_dml_parent ATTACH PARTITION partition_dml_right FOR VALUES FROM (10) TO (20);
INSERT INTO partition_dml_parent VALUES (1, 30);
CREATE TABLE partition_dml_ref
  (id integer REFERENCES partition_dml_parent(id) ON UPDATE CASCADE ON DELETE CASCADE);
INSERT INTO partition_dml_ref VALUES (1);
CREATE FUNCTION partition_dml_touch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.amount = NEW.amount + 1; RETURN NEW; END $$;
CREATE TRIGGER partition_dml_touch BEFORE UPDATE OF amount ON partition_dml_parent
  FOR EACH ROW EXECUTE FUNCTION partition_dml_touch();
UPDATE partition_dml_parent SET amount = amount + 1 WHERE id = 1 RETURNING id, amount;
UPDATE partition_dml_parent SET id = 2 WHERE id = 1 RETURNING id, amount;
SELECT id FROM partition_dml_ref;
UPDATE partition_dml_parent SET id = 12 WHERE id = 2 RETURNING id, amount;
SELECT id, amount FROM partition_dml_right;
SELECT id FROM partition_dml_ref;
DELETE FROM partition_dml_parent WHERE id = 12 RETURNING id, amount;
SELECT count(*) FROM partition_dml_ref;
DROP TABLE partition_dml_parent CASCADE;
DROP TABLE partition_dml_ref;
DROP FUNCTION partition_dml_touch();

-- Child-local composite constraints follow prepended inherited columns.
CREATE TABLE inheritance_key_parent (id integer PRIMARY KEY);
CREATE TABLE inheritance_key_lookup (extra text, marker integer, PRIMARY KEY(extra, marker));
INSERT INTO inheritance_key_lookup VALUES ('first', 7), ('second', 8);
CREATE TABLE inheritance_key_child (extra text, marker integer,
  UNIQUE(extra, marker), FOREIGN KEY(extra, marker) REFERENCES inheritance_key_lookup)
  INHERITS (inheritance_key_parent);
INSERT INTO inheritance_key_child VALUES (1, 'first', 7), (1, 'second', 8);
INSERT INTO inheritance_key_child VALUES (2, 'first', 7);
INSERT INTO inheritance_key_child VALUES (3, 'missing', 9);
UPDATE inheritance_key_parent SET id = 5;
SELECT id, extra, marker FROM ONLY inheritance_key_child ORDER BY extra;
DROP TABLE inheritance_key_parent CASCADE;
DROP TABLE inheritance_key_child;
DROP TABLE inheritance_key_lookup;
