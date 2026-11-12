-- CREATE TABLE numbers its defaults, CHECK and NOT NULL constraints, then the
-- primary key before the other keys, as catalog queries ordered by OID show.
CREATE TABLE numbered (a int, b int UNIQUE DEFAULT 2, c int UNIQUE, z int NOT NULL DEFAULT 0,
  CHECK (a > 0), UNIQUE (a, b), PRIMARY KEY (c, a), CHECK (b < 9));
SELECT conname, contype FROM pg_constraint WHERE conrelid = 'numbered'::regclass ORDER BY oid;
SELECT relname FROM pg_class WHERE relname LIKE 'numbered%' ORDER BY oid;
SELECT adnum FROM pg_attrdef WHERE adrelid = 'numbered'::regclass ORDER BY oid;
SELECT c.relname, i.indisprimary FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
 WHERE i.indrelid = 'numbered'::regclass ORDER BY i.indexrelid;
CREATE TABLE numbered_child (x int PRIMARY KEY, y int UNIQUE, w int REFERENCES numbered_child,
  UNIQUE (y, x));
SELECT conname, contype FROM pg_constraint WHERE conrelid = 'numbered_child'::regclass ORDER BY oid;
SELECT relname FROM pg_class WHERE relname LIKE 'numbered_child%' ORDER BY oid;
DROP TABLE numbered, numbered_child;
