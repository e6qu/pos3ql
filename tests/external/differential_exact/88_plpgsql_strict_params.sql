-- `#print_strict_params`, `#option dump` and `plpgsql.print_strict_params`:
-- STRICT failures list the variables a statement reads, in declaration
-- order, or EXECUTE's USING values. The settings a function is compiled
-- under are those of the session that created it.
CREATE TABLE sp88 (a int, b text);
INSERT INTO sp88 VALUES (1, 'x'), (1, 'y');
CREATE FUNCTION sp88_on(p int, q text) RETURNS text LANGUAGE plpgsql AS $$
#print_strict_params on
DECLARE r text; z int := 3;
BEGIN SELECT b INTO STRICT r FROM sp88 WHERE a = p AND b <> q AND z = 3; RETURN r; END $$;
SELECT sp88_on(1, 'n');
SELECT sp88_on(2, 'it''s');
CREATE FUNCTION sp88_off(p int) RETURNS text LANGUAGE plpgsql AS $$
#print_strict_params off
DECLARE r text;
BEGIN SELECT b INTO STRICT r FROM sp88 WHERE a = p; RETURN r; END $$;
SET plpgsql.print_strict_params = on;
SHOW plpgsql.print_strict_params;
SELECT sp88_off(1);
CREATE FUNCTION sp88_session(p int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE r int;
BEGIN UPDATE sp88 SET a = a WHERE a = p RETURNING a INTO STRICT r; RETURN r; END $$;
RESET plpgsql.print_strict_params;
SELECT sp88_session(1);
SELECT sp88_session(5);
CREATE FUNCTION sp88_execute(p text) RETURNS text LANGUAGE plpgsql AS $$
#print_strict_params on
DECLARE r text;
BEGIN EXECUTE 'SELECT b FROM sp88 WHERE a = $1' INTO STRICT r USING length(p); RETURN r; END $$;
SELECT sp88_execute('x');
CREATE FUNCTION sp88_none() RETURNS text LANGUAGE plpgsql AS $$
#print_strict_params on
DECLARE r text;
BEGIN SELECT b INTO STRICT r FROM sp88; RETURN r; END $$;
SELECT sp88_none();
CREATE FUNCTION sp88_dump() RETURNS int LANGUAGE plpgsql AS $$
#option dump
BEGIN RETURN 1; END $$;
SELECT sp88_dump();
CREATE FUNCTION sp88_bad() RETURNS int LANGUAGE plpgsql AS $$
#print_strict_params maybe
BEGIN RETURN 1; END $$;
SET plpgsql.print_strict_params = maybe;
CREATE TABLE vc88 (a int);
INSERT INTO vc88 VALUES (10);
SET plpgsql.variable_conflict = use_column;
CREATE FUNCTION vc88_column(a int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN (SELECT a FROM vc88); END $$;
CREATE FUNCTION vc88_text(a text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN RETURN (SELECT a::text FROM vc88); END $$;
RESET plpgsql.variable_conflict;
SELECT vc88_column(1);
SELECT vc88_text('x');
