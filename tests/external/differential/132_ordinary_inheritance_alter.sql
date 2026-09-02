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
