-- PL/pgSQL's extra checks, as plpgsql.extra_warnings and extra_errors
-- select them: shadowed_variables when CREATE FUNCTION validates a body,
-- too_many_rows and strict_multi_assignment when the statement runs. Then
-- the context of a failing assignment's conversion.
CREATE TABLE xc_t (a int, b text, c int);
INSERT INTO xc_t VALUES (1, 'x', 10), (2, 'y', 20);
SET plpgsql.extra_warnings = 'strict_multi_assignment';
DO $$ DECLARE a int; b text; BEGIN SELECT 1, 'q', 3 INTO a, b; RAISE NOTICE '% %', a, b; END $$;
DO $$ DECLARE a int; b text; c int; BEGIN SELECT 1 INTO a, b, c; RAISE NOTICE '% % %', a, b, c; END $$;
DO $$ DECLARE a int; BEGIN SELECT 1, 2 INTO a; END $$;
DO $$ DECLARE a int; BEGIN SELECT 1, 2 INTO a FROM xc_t WHERE false; END $$;
DO $$ DECLARE r xc_t; BEGIN SELECT 1, 'z' INTO r; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE r xc_t; BEGIN SELECT 1, 'z', 3, 4 INTO r; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE r xc_t; BEGIN SELECT * INTO r FROM xc_t LIMIT 1; END $$;
DO $$ DECLARE r record; BEGIN SELECT 1, 2 INTO r; END $$;
DO $$ DECLARE r xc_t; BEGIN r := ROW(1, 'a'); RAISE NOTICE '%', r; END $$;
DO $$ DECLARE a int; b int; BEGIN EXECUTE 'SELECT 1' INTO a, b; END $$;
DO $$ DECLARE x int; y int; BEGIN INSERT INTO xc_t VALUES (5, 'e', 50) RETURNING xc_t.a INTO x, y; END $$;
DO $$ DECLARE a int; b int; c refcursor; BEGIN OPEN c FOR SELECT 1; FETCH c INTO a, b; END $$;
SET plpgsql.extra_errors = 'strict_multi_assignment';
DO $$ DECLARE a int; b text; BEGIN SELECT 1, 'q', 3 INTO a, b; END $$;
RESET plpgsql.extra_errors;
SET plpgsql.extra_warnings = 'too_many_rows';
DO $$ DECLARE a int; n int; BEGIN SELECT xc_t.a INTO a FROM xc_t ORDER BY 1; GET DIAGNOSTICS n = ROW_COUNT; RAISE NOTICE '% %', a, n; END $$;
DO $$ DECLARE a int; BEGIN EXECUTE 'SELECT a FROM xc_t' INTO a; END $$;
DO $$ DECLARE a int; BEGIN SELECT xc_t.a INTO STRICT a FROM xc_t; END $$;
SET plpgsql.print_strict_params = on;
CREATE FUNCTION xc_rows(p int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE a int; BEGIN SELECT xc_t.a INTO a FROM xc_t WHERE xc_t.a > p; RETURN a; END $$;
SELECT xc_rows(0);
SET plpgsql.extra_errors = 'too_many_rows';
SELECT xc_rows(0);
DO $$ DECLARE a int; BEGIN SELECT xc_t.a INTO a FROM xc_t; EXCEPTION WHEN too_many_rows THEN RAISE NOTICE 'caught %', SQLERRM; END $$;
RESET plpgsql.extra_errors;
RESET plpgsql.print_strict_params;
SET plpgsql.extra_warnings = 'shadowed_variables';
CREATE FUNCTION xc_shadow(p int, OUT q int) LANGUAGE plpgsql AS $$
DECLARE
  p int;
  x int;
BEGIN
  DECLARE
    x text;
    q int;
    found bool;
  BEGIN
    NULL;
  END;
  FOR i IN 1..2 LOOP
    DECLARE i int; BEGIN NULL; END;
  END LOOP;
END $$;
CREATE FUNCTION xc_shadow_trigger() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE new int; tg_op text; BEGIN RETURN NULL; END $$;
CREATE PROCEDURE xc_shadow_label(p int) LANGUAGE plpgsql AS $$
<<blk>> DECLARE blk int; BEGIN NULL; END $$;
DO $$ DECLARE found int; BEGIN NULL; END $$;
SET plpgsql.extra_errors = 'shadowed_variables';
CREATE FUNCTION xc_shadow_error(p int) RETURNS int LANGUAGE plpgsql AS $$ DECLARE p int; BEGIN RETURN 1; END $$;
SET check_function_bodies = off;
CREATE FUNCTION xc_shadow_unchecked(p int) RETURNS int LANGUAGE plpgsql AS $$ DECLARE p int; BEGIN RETURN 1; END $$;
SELECT xc_shadow_unchecked(1);
RESET check_function_bodies;
RESET plpgsql.extra_errors;
RESET plpgsql.extra_warnings;
-- A literal that fails its input is positioned in the assignment; a type
-- modifier failure or a folded immutable call has the assignment's context.
DO $$ DECLARE v varchar(5); BEGIN v := 'abcdefgh'; END $$;
DO $$ DECLARE v varchar(5); BEGIN v := upper('abcdefgh'); END $$;
DO $$ DECLARE v int; BEGIN v := 'zz'; END $$;
DO $$ DECLARE v int; BEGIN v := 'zz' || ''; END $$;
DO $$ DECLARE n numeric(3,1); BEGIN n := '123.45'; END $$;
DO $$ DECLARE i int; BEGIN i := '99999999999'; END $$;
DO $$ DECLARE v varchar(3); BEGIN v := repeat('x', 5); END $$;
DO $$ DECLARE b bit(3); BEGIN b := B'10'; END $$;
CREATE TYPE xc_pair AS (s varchar(2), n int);
DO $$ DECLARE r xc_pair; BEGIN r.s := 'abc'; END $$;
DO $$ DECLARE r xc_pair; BEGIN r.n := 'x'; END $$;
CREATE TABLE xc_n (n int, s varchar(3));
CREATE FUNCTION xc_n_trg() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.n = 1 THEN NEW.n := 'zz'; END IF;
  IF NEW.n = 2 THEN NEW.s := 'abcdef'; END IF;
  IF NEW.n = 3 THEN NEW.s := upper('abcdef'); END IF;
  IF NEW.n = 4 THEN NEW.n := upper('zz'); END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER xc_n_t BEFORE INSERT ON xc_n FOR EACH ROW EXECUTE FUNCTION xc_n_trg();
INSERT INTO xc_n VALUES (1, 'a');
INSERT INTO xc_n VALUES (2, 'a');
INSERT INTO xc_n VALUES (3, 'a');
INSERT INTO xc_n VALUES (4, 'a');
