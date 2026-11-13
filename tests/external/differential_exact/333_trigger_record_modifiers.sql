-- A trigger's returned plain record keeps its fields' type modifiers from
-- query columns, casts and row values, and PostgreSQL compares them with the
-- table's; a constant that fails a NEW field's modifier fails while the
-- assignment is planned.
CREATE SCHEMA trigger_record_modifiers;
SET search_path = trigger_record_modifiers, public;
CREATE TABLE t (v varchar(5), n numeric(4,1), c char(2));
CREATE TABLE wide (v varchar(10), n numeric(5,2), c char(3));
CREATE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN SELECT 'ab'::varchar(10), 1.2::numeric(4,1), 'xy'::char(2) INTO r; RETURN r; END $$;
CREATE TRIGGER r BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION ret();
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN SELECT 'ab'::varchar(5), 1.2::numeric(5,1), 'xy'::char(2) INTO r; RETURN r; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN SELECT 'ab'::varchar(5), 1.2::numeric(4,1), 'xy'::char(2) INTO r; RETURN r; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN SELECT * FROM wide INTO r; RETURN r; END $$;
INSERT INTO wide VALUES ('abc', 1.25, 'xyz');
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN r := ROW('ab'::varchar(5), 1.2::numeric(4,1), 'xy'::char(2)); RETURN r; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN r := NEW; RETURN r; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.v := 'abcdefgh'; RETURN NEW; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.n := 12345.6; RETURN NEW; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE x text := 'abcdefgh'; BEGIN NEW.v := x; RETURN NEW; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
CREATE OR REPLACE FUNCTION ret() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN SELECT v, n, c FROM t INTO r; RETURN r; END $$;
INSERT INTO t VALUES ('a', 1, 'b');
SELECT * FROM t;
CREATE TABLE t2 (v varchar(5), n numeric(4,1));
CREATE TABLE w (v varchar(10), n numeric(4,1));
INSERT INTO w VALUES ('ab', 1.5);
CREATE FUNCTION ret2() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; c CURSOR FOR SELECT v, n FROM t2 LIMIT 1; BEGIN OPEN c; FETCH c INTO r; CLOSE c; RETURN r; END $$;
CREATE TRIGGER r2 BEFORE INSERT ON t2 FOR EACH ROW EXECUTE FUNCTION ret2();
INSERT INTO t2 SELECT 'x', 1 WHERE false;
CREATE OR REPLACE FUNCTION ret2() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; c CURSOR FOR SELECT v, n FROM w; BEGIN OPEN c; FETCH c INTO r; CLOSE c; RETURN r; END $$;
INSERT INTO t2 VALUES ('a', 1);
CREATE OR REPLACE FUNCTION ret2() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN FOR r IN SELECT v, n FROM w LOOP RETURN r; END LOOP; RETURN NULL; END $$;
INSERT INTO t2 VALUES ('a', 1);
CREATE OR REPLACE FUNCTION ret2() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN FOR r IN EXECUTE 'SELECT v::varchar(5), n FROM w' LOOP RETURN r; END LOOP; RETURN NULL; END $$;
INSERT INTO t2 VALUES ('a', 1);
CREATE OR REPLACE FUNCTION ret2() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; BEGIN EXECUTE 'SELECT v, n FROM w' INTO r; RETURN r; END $$;
INSERT INTO t2 VALUES ('a', 1);
SELECT * FROM t2;
RESET search_path;
DROP SCHEMA trigger_record_modifiers CASCADE;
