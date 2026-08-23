-- ALTER TABLE ... ALTER COLUMN ... TYPE: change a column's type, rewriting the
-- stored rows. Without USING the value casts through the assignment cast; a
-- cast that is explicit-only is refused with 42804; USING evaluates an
-- expression over the old row. Matches PostgreSQL 18 (COLUMN keyword and the
-- `SET DATA TYPE` spelling both optional/accepted).
DROP TABLE IF EXISTS ct;
CREATE TABLE ct (id int, a int, b text, c text);
INSERT INTO ct VALUES (1, 42, '3.5', 'hello'), (2, 100, '9', 'yo');

-- Assignment casts need no USING: int widens to bigint, then to text.
ALTER TABLE ct ALTER COLUMN a TYPE bigint;
SELECT a, pg_typeof(a) FROM ct ORDER BY id;
ALTER TABLE ct ALTER COLUMN a SET DATA TYPE text;
SELECT a, pg_typeof(a) FROM ct ORDER BY id;
SELECT attcollation FROM pg_attribute
 WHERE attrelid = 'ct'::regclass AND attname = 'a';
ALTER TABLE ct ALTER COLUMN a TYPE varchar(8) COLLATE "POSIX";
SELECT a, pg_typeof(a) FROM ct ORDER BY id;

-- text -> numeric is explicit-only, so it needs USING.
ALTER TABLE ct ALTER COLUMN b TYPE numeric;
ALTER TABLE ct ALTER COLUMN b TYPE numeric USING b::numeric;
SELECT b, pg_typeof(b) FROM ct ORDER BY id;

-- USING may be any expression over the old row.
ALTER TABLE ct ALTER COLUMN c TYPE int USING length(c);
SELECT c, pg_typeof(c) FROM ct ORDER BY id;

-- A type modifier is applied during the rewrite.
ALTER TABLE ct ALTER COLUMN b TYPE varchar(4);
SELECT b, pg_typeof(b) FROM ct ORDER BY id;

-- numeric -> int rounds each stored value (and persists).
DROP TABLE IF EXISTS nt;
CREATE TABLE nt (id int, n numeric);
INSERT INTO nt VALUES (1, 3.7), (2, 2.5), (3, -0.5);
ALTER TABLE nt ALTER COLUMN n TYPE int;
SELECT id, n, pg_typeof(n) FROM nt ORDER BY id;

-- Assignment-cast boundaries: date<->int and int->date are explicit (42804);
-- any type casts to text; unknown column and type errors.
DROP TABLE IF EXISTS et;
CREATE TABLE et (i int, d date, u uuid);
ALTER TABLE et ALTER COLUMN i TYPE date;
ALTER TABLE et ALTER COLUMN d TYPE int;
ALTER TABLE et ALTER COLUMN u TYPE text;
ALTER TABLE et ALTER COLUMN i TYPE int COLLATE "C";
ALTER TABLE et ALTER COLUMN nope TYPE int;
ALTER TABLE et ALTER COLUMN i TYPE nosuchtype;
SELECT pg_typeof(u) FROM et LIMIT 1;

DROP TABLE ct;
DROP TABLE nt;
DROP TABLE et;
