-- Deferrable constraint timing, lifecycle, and catalog state.
DROP TABLE IF EXISTS constraint_lifecycle_child, constraint_lifecycle_parent CASCADE;

CREATE TABLE constraint_lifecycle_parent (id integer PRIMARY KEY);
CREATE TABLE constraint_lifecycle_child (
  id integer,
  key_value integer,
  parent_id integer,
  slot int4range,
  active boolean,
  CONSTRAINT constraint_lifecycle_key UNIQUE (key_value)
    DEFERRABLE INITIALLY DEFERRED,
  CONSTRAINT constraint_lifecycle_fk FOREIGN KEY (parent_id)
    REFERENCES constraint_lifecycle_parent(id)
    DEFERRABLE INITIALLY DEFERRED,
  CONSTRAINT constraint_lifecycle_exclusion EXCLUDE USING gist
    (slot WITH &&) WHERE (active)
    DEFERRABLE INITIALLY DEFERRED
);
INSERT INTO constraint_lifecycle_parent VALUES (1);

-- A deferred foreign key can be repaired before commit.
BEGIN;
INSERT INTO constraint_lifecycle_child VALUES (1, 1, 2, '[1,4)', true);
INSERT INTO constraint_lifecycle_parent VALUES (2);
COMMIT;

-- Switching to IMMEDIATE validates obligations already raised by this transaction.
BEGIN;
INSERT INTO constraint_lifecycle_child VALUES (2, 1, 1, '[8,9)', false);
SET CONSTRAINTS constraint_lifecycle_key IMMEDIATE;
ROLLBACK;

-- Exclusion conflicts are checked at the deferred boundary too.
BEGIN;
INSERT INTO constraint_lifecycle_child VALUES
  (2, 2, 1, '[10,14)', true),
  (3, 3, 1, '[12,16)', true);
COMMIT;

-- NOT VALID exempts old rows from validation, but still enforces new writes.
INSERT INTO constraint_lifecycle_child VALUES (-1, 4, 1, '[20,21)', false);
ALTER TABLE constraint_lifecycle_child ADD CONSTRAINT constraint_lifecycle_check
  CHECK (id > 0) NOT VALID;
INSERT INTO constraint_lifecycle_child VALUES (-2, 5, 1, '[22,23)', false);
ALTER TABLE constraint_lifecycle_child VALIDATE CONSTRAINT constraint_lifecycle_check;
UPDATE constraint_lifecycle_child SET id = 4 WHERE id = -1;
ALTER TABLE constraint_lifecycle_child VALIDATE CONSTRAINT constraint_lifecycle_check;

-- Enabling enforcement validates rows that were accepted while disabled.
INSERT INTO constraint_lifecycle_child VALUES (50, 6, 1, '[24,25)', false);
ALTER TABLE constraint_lifecycle_child ADD CONSTRAINT constraint_lifecycle_bounded
  CHECK (id < 10) NOT ENFORCED;
ALTER TABLE constraint_lifecycle_child ALTER CONSTRAINT constraint_lifecycle_bounded ENFORCED;
UPDATE constraint_lifecycle_child SET id = 6 WHERE id = 50;
ALTER TABLE constraint_lifecycle_child ALTER CONSTRAINT constraint_lifecycle_bounded ENFORCED;
ALTER TABLE constraint_lifecycle_child DROP CONSTRAINT constraint_lifecycle_bounded;
ALTER TABLE constraint_lifecycle_child ADD CONSTRAINT constraint_lifecycle_bounded
  CHECK (id < 10);

-- PostgreSQL 18 permits enforceability changes only for foreign keys.
CREATE TABLE constraint_enforcement_parent (id integer PRIMARY KEY);
CREATE TABLE constraint_enforcement_child (
  parent_id integer REFERENCES constraint_enforcement_parent(id) NOT ENFORCED
);
INSERT INTO constraint_enforcement_child VALUES (99);
ALTER TABLE constraint_enforcement_child
  ALTER CONSTRAINT constraint_enforcement_child_parent_id_fkey ENFORCED;
INSERT INTO constraint_enforcement_parent VALUES (99);
ALTER TABLE constraint_enforcement_child
  ALTER CONSTRAINT constraint_enforcement_child_parent_id_fkey ENFORCED;
INSERT INTO constraint_enforcement_child VALUES (100);
SELECT convalidated, conenforced FROM pg_constraint
WHERE conname = 'constraint_enforcement_child_parent_id_fkey';

-- A referenced key owns its dependent foreign key for RESTRICT/CASCADE.
CREATE TABLE constraint_drop_parent (
  id integer,
  CONSTRAINT constraint_drop_key UNIQUE (id)
);
ALTER TABLE constraint_drop_parent ADD CONSTRAINT constraint_drop_spare UNIQUE (id);
CREATE TABLE constraint_drop_child (
  parent_id integer REFERENCES constraint_drop_parent(id)
);
ALTER TABLE constraint_drop_parent DROP CONSTRAINT constraint_drop_spare RESTRICT;
ALTER TABLE constraint_drop_parent DROP CONSTRAINT constraint_drop_key RESTRICT;
ALTER TABLE constraint_drop_parent DROP CONSTRAINT constraint_drop_key CASCADE;
SELECT count(*) FROM pg_constraint
WHERE conrelid = 'constraint_drop_child'::regclass AND contype = 'f';

ALTER TABLE constraint_lifecycle_child RENAME COLUMN active TO enabled;
SELECT pg_get_constraintdef(oid) FROM pg_constraint
WHERE conname = 'constraint_lifecycle_exclusion';
ALTER TABLE constraint_lifecycle_child RENAME COLUMN enabled TO active;

SELECT conname, contype, condeferrable, condeferred, convalidated, conenforced
FROM pg_constraint
WHERE conrelid = 'constraint_lifecycle_child'::regclass
ORDER BY conname;
SELECT pg_get_constraintdef(oid)
FROM pg_constraint
WHERE conname IN ('constraint_lifecycle_key', 'constraint_lifecycle_exclusion')
ORDER BY conname;

DROP TABLE constraint_drop_child, constraint_drop_parent,
  constraint_enforcement_child, constraint_enforcement_parent,
  constraint_lifecycle_child, constraint_lifecycle_parent;
