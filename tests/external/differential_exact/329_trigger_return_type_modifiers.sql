-- A BEFORE trigger's returned row must match the table's column types,
-- type modifiers included: a varchar(10) field does not fit varchar(5).
CREATE SCHEMA trigger_return_modifiers;
SET search_path = trigger_return_modifiers, public;
CREATE TABLE t (v varchar(5), n numeric(4,1), c char(2));
CREATE TABLE wide (v varchar(10), n numeric(5,2), c char(3));
CREATE TABLE same (v varchar(5), n numeric(4,1), c char(2));
CREATE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r wide; BEGIN r := ROW('abc', 1.25, 'xy'); RETURN r; END $$;
CREATE TRIGGER r BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION ret();
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r same; BEGIN r := ROW('abc', 1.2, 'xy'); RETURN r; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RETURN ROW('abc', 1.2, 'xy')::record; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
SELECT * FROM t;
RESET search_path;
DROP SCHEMA trigger_return_modifiers CASCADE;
