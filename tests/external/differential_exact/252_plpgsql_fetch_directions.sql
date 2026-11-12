-- PL/pgSQL FETCH and MOVE directions follow PostgreSQL's grammar: counts and
-- positions are expressions, a FETCH cannot move by more than one row, and
-- the cursor must be a declared refcursor variable.
CREATE TABLE m252 (v int);
INSERT INTO m252 SELECT generate_series(1, 10);
DO $$ DECLARE c refcursor; x int; n int := 3; BEGIN
  OPEN c SCROLL FOR SELECT v FROM m252 ORDER BY v;
  FETCH ABSOLUTE n + 1 FROM c INTO x; RAISE NOTICE 'absolute %', x;
  FETCH RELATIVE -n FROM c INTO x; RAISE NOTICE 'relative %', x;
  MOVE FORWARD n IN c; FETCH c INTO x; RAISE NOTICE 'forward %', x;
  MOVE BACKWARD 2 * n FROM c; FETCH c INTO x; RAISE NOTICE 'backward %', x;
  MOVE BACKWARD 0 FROM c; FETCH c INTO x; RAISE NOTICE 'backward zero %', x;
  MOVE abs(-2) FROM c; FETCH c INTO x; RAISE NOTICE 'count %', x;
  MOVE ABSOLUTE '2' IN c; FETCH c INTO x; RAISE NOTICE 'text %', x;
  MOVE ABSOLUTE 2.7 IN c; FETCH NEXT FROM c INTO x; RAISE NOTICE 'numeric %', x;
  FETCH FORWARD FROM c INTO x; FETCH BACKWARD IN c INTO x; RAISE NOTICE 'bare %', x;
  MOVE FORWARD ALL FROM c; MOVE BACKWARD ALL FROM c; FETCH FROM c INTO x; RAISE NOTICE 'all %', x;
  n := NULL;
  MOVE RELATIVE n IN c;
END $$;
DO $$ DECLARE c refcursor; BEGIN OPEN c SCROLL FOR SELECT v FROM m252; MOVE ABSOLUTE 'abc' IN c; END $$;
CREATE FUNCTION f252a() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; x int; BEGIN
  FETCH BACKWARD 1 FROM c INTO x; END $$;
CREATE FUNCTION f252b() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; x int; BEGIN
  FETCH ALL c INTO x; END $$;
CREATE FUNCTION f252c() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; x int; BEGIN
  FETCH 2 IN c INTO x; END $$;
CREATE FUNCTION f252d() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; x int; BEGIN
  FETCH FORWARD ALL FROM c INTO x; END $$;
CREATE FUNCTION f252e() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; x int; BEGIN
  FETCH NEXT c INTO x; END $$;
CREATE FUNCTION f252f() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; x int; BEGIN
  FETCH FORWARD c INTO x; END $$;
CREATE FUNCTION f252g() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; BEGIN MOVE LAST c; END $$;
CREATE FUNCTION f252h() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; BEGIN MOVE 3 c; END $$;
CREATE FUNCTION f252i() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c int; x int; BEGIN FETCH c INTO x; END $$;
CREATE FUNCTION f252j() RETURNS void LANGUAGE plpgsql AS $$ DECLARE x int; BEGIN FETCH nope INTO x; END $$;
CREATE FUNCTION f252k() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c int; BEGIN OPEN c FOR SELECT 1; END $$;
CREATE FUNCTION f252l() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c int; BEGIN CLOSE c; END $$;
CREATE FUNCTION f252m() RETURNS void LANGUAGE plpgsql AS $$ BEGIN CLOSE nope; END $$;
CREATE FUNCTION f252n() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; d refcursor; BEGIN MOVE c FROM d; END $$;
CREATE FUNCTION f252o() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; BEGIN MOVE foo FROM c; END $$;
CREATE FUNCTION f252p() RETURNS void LANGUAGE plpgsql AS $$ DECLARE r record; BEGIN CLOSE r; END $$;
CREATE FUNCTION f252q() RETURNS void LANGUAGE plpgsql AS $$ DECLARE c refcursor; n int; BEGIN MOVE n FROM c; END $$;
SELECT proname FROM pg_proc WHERE proname LIKE 'f252%' ORDER BY proname;
