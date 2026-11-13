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
-- Keys after the primary key follow declaration order, whether written on a
-- column or as table constraints; NOT NULL written on a column (or implied
-- by its PRIMARY KEY) is numbered in column order, before a table primary
-- key's other columns.
CREATE TABLE n1 (a int, b int, UNIQUE (a, b), c int UNIQUE);
SELECT conname, contype FROM pg_constraint WHERE conrelid = 'n1'::regclass ORDER BY oid;
SELECT relname FROM pg_class WHERE relname LIKE 'n1%' ORDER BY oid;
CREATE TABLE n2 (a int, b int, UNIQUE (b), c int UNIQUE, UNIQUE (a));
SELECT conname, contype FROM pg_constraint WHERE conrelid = 'n2'::regclass ORDER BY oid;
SELECT relname FROM pg_class WHERE relname LIKE 'n2%' ORDER BY oid;
CREATE TABLE n3 (a int NOT NULL, b int, c int NOT NULL, PRIMARY KEY (c, b), d int NOT NULL);
SELECT conname, contype FROM pg_constraint WHERE conrelid = 'n3'::regclass ORDER BY oid;
CREATE TABLE n5 (a int PRIMARY KEY, b int NOT NULL, c int CHECK (c > 0), CHECK (a > 0),
  d int CHECK (d > 0) NOT NULL, e int UNIQUE, CONSTRAINT n5_named UNIQUE (b),
  f int CONSTRAINT n5_f_named UNIQUE, UNIQUE (f, e), g int UNIQUE);
SELECT conname, contype FROM pg_constraint WHERE conrelid = 'n5'::regclass ORDER BY oid;
SELECT relname FROM pg_class WHERE relname LIKE 'n5%' ORDER BY oid;
DROP TABLE n1, n2, n3, n5;
