-- Exclusion constraints over user-defined functions, types, operators and
-- operator classes, with explicit operator classes and COLLATE.
CREATE FUNCTION fold(text) RETURNS text LANGUAGE sql IMMUTABLE AS 'SELECT lower($1)';
CREATE FUNCTION kept(text) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT $1 <> ''skip''';
CREATE TABLE folded (s text, EXCLUDE USING btree (fold(s) WITH =) WHERE (kept(s)));
INSERT INTO folded VALUES ('Ab'), ('skip'), ('skip');
INSERT INTO folded VALUES ('aB');
SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'folded'::regclass;
SELECT pg_describe_object(classid, objid, objsubid) AS dependent, pg_describe_object(refclassid, refobjid, refobjsubid) AS referenced, deptype FROM pg_depend WHERE refobjid IN ('fold'::regproc, 'kept'::regproc) ORDER BY 1, 2;
DROP FUNCTION fold(text);
ALTER FUNCTION fold(text) RENAME TO fold2;
SET search_path = pg_catalog;
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'public.folded'::regclass;
\d public.folded
RESET search_path;
INSERT INTO folded VALUES ('AB');
ALTER TABLE folded RENAME CONSTRAINT folded_fold_excl TO folded_once;
INSERT INTO folded VALUES ('ab');
DROP FUNCTION kept(text) CASCADE;
\d folded
CREATE TYPE mood AS ENUM ('sad', 'ok');
CREATE TABLE moods (m mood, EXCLUDE USING btree (m WITH =) WHERE (m <> 'sad'::mood));
INSERT INTO moods VALUES ('sad'), ('sad'), ('ok');
INSERT INTO moods VALUES ('ok');
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'moods'::regclass;
DROP TYPE mood CASCADE;
-- A user btree operator class and its operators.
CREATE FUNCTION abs_eq(int4, int4) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT abs($1) = abs($2)';
CREATE FUNCTION abs_lt(int4, int4) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT abs($1) < abs($2)';
CREATE FUNCTION abs_cmp(int4, int4) RETURNS int4 LANGUAGE sql IMMUTABLE AS 'SELECT CASE WHEN abs($1) < abs($2) THEN -1 WHEN abs($1) > abs($2) THEN 1 ELSE 0 END';
CREATE OPERATOR =| (LEFTARG = int4, RIGHTARG = int4, FUNCTION = abs_eq, COMMUTATOR = =|);
CREATE OPERATOR <| (LEFTARG = int4, RIGHTARG = int4, FUNCTION = abs_lt);
CREATE OPERATOR CLASS abs_ops FOR TYPE int4 USING btree AS OPERATOR 1 <|, OPERATOR 3 =|, FUNCTION 1 abs_cmp(int4, int4);
CREATE TABLE absolute (n int4, EXCLUDE USING btree (n abs_ops WITH =|));
INSERT INTO absolute VALUES (1), (-2);
INSERT INTO absolute VALUES (-1);
INSERT INTO absolute VALUES (2);
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'absolute'::regclass;
SELECT indclass[0] = (SELECT oid FROM pg_opclass WHERE opcname = 'abs_ops') AS user_class FROM pg_index WHERE indrelid = 'absolute'::regclass;
SELECT conexclop[1] = '=|(int4,int4)'::regoperator::oid AS user_operator FROM pg_constraint WHERE conrelid = 'absolute'::regclass;
CREATE TABLE absolute2 (n int4, EXCLUDE USING btree (n WITH =|));
CREATE TABLE absolute3 (n int4, EXCLUDE USING btree (n abs_ops WITH <|));
CREATE TABLE absolute4 (n int4, EXCLUDE USING btree (n abs_ops WITH =));
DROP OPERATOR =| (int4, int4);
\d absolute
DROP OPERATOR CLASS abs_ops USING btree CASCADE;
\d absolute
-- Explicit built-in operator classes and collations.
CREATE TABLE patterned (s text, v varchar(10), b char(3), EXCLUDE USING btree (s text_pattern_ops WITH =), EXCLUDE USING hash (v varchar_pattern_ops WITH =), EXCLUDE USING btree (b bpchar_pattern_ops WITH =));
INSERT INTO patterned VALUES ('a', 'a', 'a');
INSERT INTO patterned VALUES ('a', 'b', 'b');
INSERT INTO patterned VALUES ('b', 'a', 'c');
INSERT INTO patterned VALUES ('c', 'c', 'a  ');
SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'patterned'::regclass ORDER BY 1;
SELECT c.relname, i.indclass::regclass[]::text, i.indcollation FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'patterned'::regclass ORDER BY 1;
CREATE TABLE collated (s text COLLATE "C", t text, EXCLUDE USING btree (s COLLATE "POSIX" WITH =, t COLLATE "C" WITH =), EXCLUDE USING btree ((s || t) COLLATE "C" WITH =));
SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'collated'::regclass ORDER BY 1;
SELECT c.relname, i.indcollation::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'collated'::regclass ORDER BY 1;
\d collated
INSERT INTO collated VALUES ('x', 'y'), ('x', 'y');
INSERT INTO collated VALUES ('x', 'z');
CREATE TABLE badcoll (n int4, EXCLUDE USING btree (n COLLATE "C" WITH =));
CREATE TABLE badclass (n int4, EXCLUDE USING btree (n text_pattern_ops WITH =));
CREATE TABLE badclass2 (s text, EXCLUDE USING btree (s nope_ops WITH =));
CREATE TABLE badclass3 (s text, EXCLUDE USING btree (s text_pattern_ops WITH <>));
CREATE TABLE gisty (r int4range, EXCLUDE USING gist (r range_ops WITH &&));
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'gisty'::regclass;
-- Partitions and LIKE copies keep what their elements were bound to.
CREATE FUNCTION fold(text) RETURNS text LANGUAGE sql IMMUTABLE AS 'SELECT lower($1)';
CREATE TABLE parted (k int4, s text, EXCLUDE USING btree (k WITH =, fold(s) WITH =)) PARTITION BY RANGE (k);
CREATE TABLE parted1 PARTITION OF parted FOR VALUES FROM (0) TO (10);
INSERT INTO parted VALUES (1, 'A');
INSERT INTO parted VALUES (1, 'a');
CREATE TABLE copied (LIKE parted INCLUDING ALL);
INSERT INTO copied VALUES (1, 'A'), (1, 'a');
SELECT conrelid::regclass, conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE contype = 'x' AND conrelid::regclass::text IN ('parted', 'parted1', 'copied') ORDER BY 1, 2;
SELECT pg_describe_object(classid, objid, objsubid) AS dependent, deptype FROM pg_depend WHERE refobjid = 'fold'::regproc ORDER BY 1;
ALTER TABLE parted ALTER COLUMN s TYPE varchar(20);
INSERT INTO parted VALUES (1, 'A');
DROP FUNCTION fold(text) CASCADE;
SELECT conrelid::regclass, conname FROM pg_constraint WHERE contype = 'x' AND conrelid::regclass::text IN ('parted', 'parted1', 'copied') ORDER BY 1, 2;
-- Btree elements keep their ordering options; other methods reject them.
CREATE TABLE descs (a int4, b int4, EXCLUDE USING btree (a DESC WITH =, b NULLS FIRST WITH =, a DESC NULLS LAST WITH =));
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'descs'::regclass;
SELECT indoption FROM pg_index WHERE indrelid = 'descs'::regclass;
CREATE TABLE descg (r int4range, EXCLUDE USING gist (r DESC WITH &&));
