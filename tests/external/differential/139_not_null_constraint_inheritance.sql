DROP TABLE IF EXISTS not_null_control_child;
DROP TABLE IF EXISTS not_null_control_parent;

CREATE TABLE not_null_control_parent (id integer NOT NULL);
CREATE TABLE not_null_control_child (extra integer)
  INHERITS (not_null_control_parent);

SELECT conislocal, coninhcount, connoinherit
  FROM pg_constraint
 WHERE conrelid = 'not_null_control_child'::regclass
   AND conname = 'not_null_control_child_id_not_null';

ALTER TABLE not_null_control_parent
  ALTER CONSTRAINT not_null_control_parent_id_not_null NO INHERIT;
SELECT conislocal, coninhcount, connoinherit
  FROM pg_constraint
 WHERE conrelid = 'not_null_control_parent'::regclass
   AND conname = 'not_null_control_parent_id_not_null';
SELECT conislocal, coninhcount, connoinherit
  FROM pg_constraint
 WHERE conrelid = 'not_null_control_child'::regclass
   AND conname = 'not_null_control_child_id_not_null';

ALTER TABLE not_null_control_parent
  ALTER CONSTRAINT not_null_control_parent_id_not_null INHERIT;
SELECT conislocal, coninhcount, connoinherit
  FROM pg_constraint
 WHERE conrelid = 'not_null_control_child'::regclass
   AND conname = 'not_null_control_child_id_not_null';

DROP TABLE not_null_control_child;
DROP TABLE not_null_control_parent;
