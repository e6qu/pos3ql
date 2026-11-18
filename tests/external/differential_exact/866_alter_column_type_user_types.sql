-- ALTER COLUMN TYPE to a domain, enum or composite rewrites the column through
-- the type: a domain's checks and NOT NULL apply to every row, an enum or
-- composite needs USING unless the column already has the type, and the
-- column takes the type's collation unless COLLATE gives one.
CREATE DOMAIN short866 AS text CHECK (length(VALUE) < 4);
CREATE DOMAIN pos866 AS integer CHECK (VALUE > 0);
CREATE DOMAIN req866 AS text NOT NULL;
CREATE DOMAIN cs866 AS text COLLATE "C";
CREATE TYPE mood866 AS ENUM ('sad', 'ok', 'happy');
CREATE TYPE pair866 AS (a integer, b text);
CREATE TABLE t866 (id integer, t text, n integer, e text, p integer, v text, k text);
INSERT INTO t866 VALUES (1, 'abc', 5, 'ok', 1, 'x', 'q'), (2, 'de', 7, 'happy', 2, NULL, 'r');
SELECT id, t, n, e FROM t866 ORDER BY id;
-- A domain over the column's type applies its checks to each row.
ALTER TABLE t866 ALTER COLUMN t TYPE short866;
INSERT INTO t866 VALUES (3, 'wxyz', 1, 'sad', 3, 'y', 's');
UPDATE t866 SET t = 'abcd' WHERE id = 1;
ALTER TABLE t866 ALTER COLUMN n TYPE pos866;
UPDATE t866 SET n = 0 WHERE id = 1;
SELECT attname, atttypid::regtype, attnotnull FROM pg_attribute
  WHERE attrelid = 't866'::regclass AND attnum > 0 ORDER BY attnum;
-- A failing row leaves the column as it was.
UPDATE t866 SET n = 0, t = 'zzzzz' WHERE false;
ALTER TABLE t866 ALTER COLUMN p TYPE pos866 USING p - 1;
ALTER TABLE t866 ALTER COLUMN v TYPE req866;
ALTER TABLE t866 ALTER COLUMN v TYPE req866 USING coalesce(v, 'none');
ALTER TABLE t866 ALTER COLUMN k TYPE cs866;
-- An enum needs USING.
ALTER TABLE t866 ALTER COLUMN e TYPE mood866;
ALTER TABLE t866 ALTER COLUMN e TYPE mood866 USING e::mood866;
ALTER TABLE t866 ALTER COLUMN id TYPE mood866 USING 'ok';
SELECT id, e FROM t866 ORDER BY e DESC, id;
ALTER TABLE t866 ALTER COLUMN e TYPE mood866;
ALTER TABLE t866 ALTER COLUMN e TYPE text;
ALTER TABLE t866 ALTER COLUMN e TYPE mood866 USING e::mood866;
-- A composite needs USING.
ALTER TABLE t866 ADD COLUMN c integer DEFAULT 4;
ALTER TABLE t866 ALTER COLUMN c TYPE pair866;
ALTER TABLE t866 ALTER COLUMN c TYPE pair866 USING ROW(c, 'z')::pair866;
SELECT id, c FROM t866 ORDER BY id;
-- Defaults, indexes and collations follow the new type.
ALTER TABLE t866 ADD COLUMN d text DEFAULT 'ab';
ALTER TABLE t866 ALTER COLUMN d TYPE short866;
ALTER TABLE t866 ALTER COLUMN d SET DEFAULT 'toolong';
INSERT INTO t866 (id, t, n, e, p, v, k) VALUES (9, 'a', 1, 'sad', 1, 'z', 'k');
ALTER TABLE t866 ALTER COLUMN d SET DEFAULT 'ok';
CREATE INDEX ON t866 (t);
ALTER TABLE t866 ALTER COLUMN t TYPE text COLLATE "C";
ALTER TABLE t866 ALTER COLUMN t TYPE cs866 COLLATE "POSIX";
SELECT attname, atttypid::regtype, attcollation::regcollation FROM pg_attribute
  WHERE attrelid = 't866'::regclass AND attnum > 0 ORDER BY attnum;
SELECT id, t, n, e, p, v, k, c, d FROM t866 ORDER BY id;
ALTER TABLE t866 ALTER COLUMN t TYPE nosuch866;
ALTER TABLE t866 ALTER COLUMN t TYPE pos866;
ALTER TABLE t866 ALTER COLUMN t TYPE pos866 USING length(t);
