-- A PL/pgSQL variable keeps the datum it is given: a simple expression's
-- result as the expression formed it (a four-byte header), and a query's
-- result, or an expression holding a subquery, as its result tuple holds it
-- (a short value packed, a stored value in its TOAST form). A record's
-- fields keep the forms of the row it was given.
CREATE TABLE pt (id int, s text, big text);
INSERT INTO pt VALUES (1, 'short', repeat('a', 3000)), (2, 'x', repeat(md5('q'), 300));
CREATE FUNCTION forms(k int, e text) RETURNS int LANGUAGE plpgsql AS $$
DECLARE
  a text; c text; d text; f text; g text; i text; r record; j text; v text;
  w text; y text; z text; dflt text := 'lit'; q text; copied text := dflt;
BEGIN
  SELECT 'lit'::text INTO a;
  c := upper('abc');
  d := (SELECT upper(s) FROM pt WHERE id = k);
  f := c;
  SELECT s || '' INTO g FROM pt WHERE id = k;
  i := 'lit' || (SELECT 'x');
  SELECT s, big INTO r FROM pt WHERE id = k;
  j := r.s;
  SELECT s INTO v FROM pt WHERE id = k;
  SELECT big INTO w FROM pt WHERE id = k;
  y := 'lit';
  z := v || '';
  q := (SELECT s FROM pt WHERE id = k);
  RETURN CASE e
    WHEN 'a' THEN pg_column_size(a) WHEN 'c' THEN pg_column_size(c)
    WHEN 'd' THEN pg_column_size(d) WHEN 'f' THEN pg_column_size(f)
    WHEN 'g' THEN pg_column_size(g) WHEN 'i' THEN pg_column_size(i)
    WHEN 'r.s' THEN pg_column_size(r.s) WHEN 'r.big' THEN pg_column_size(r.big)
    WHEN 'j' THEN pg_column_size(j) WHEN 'v' THEN pg_column_size(v)
    WHEN 'w' THEN pg_column_size(w) WHEN 'y' THEN pg_column_size(y)
    WHEN 'z' THEN pg_column_size(z) WHEN 'dflt' THEN pg_column_size(dflt)
    WHEN 'q' THEN pg_column_size(q) WHEN 'copied' THEN pg_column_size(copied)
  END;
END $$;
SELECT id, e, forms(id, e)
FROM pt, unnest(ARRAY['a', 'c', 'd', 'f', 'g', 'i', 'r.s', 'r.big', 'j', 'v',
                      'w', 'y', 'z', 'dflt', 'q', 'copied']) WITH ORDINALITY u(e, n)
ORDER BY id, n;
-- A FOR loop's variable holds each row as the query's result tuple holds
-- it, through a static or an EXECUTE query, a scalar, a record or a list of
-- targets.
CREATE FUNCTION for_forms(e text) RETURNS SETOF int LANGUAGE plpgsql AS $$
DECLARE v text; r record; a text; b text;
BEGIN
  IF e = 'scalar' THEN
    FOR v IN SELECT big FROM pt ORDER BY id LOOP RETURN NEXT pg_column_size(v); END LOOP;
  ELSIF e = 'short' THEN
    FOR v IN SELECT s FROM pt ORDER BY id LOOP RETURN NEXT pg_column_size(v); END LOOP;
  ELSIF e = 'record' THEN
    FOR r IN SELECT s, big FROM pt ORDER BY id LOOP
      RETURN NEXT pg_column_size(r.big); RETURN NEXT pg_column_size(r.s);
    END LOOP;
  ELSIF e = 'targets' THEN
    FOR a, b IN SELECT s, big FROM pt ORDER BY id LOOP
      RETURN NEXT pg_column_size(a); RETURN NEXT pg_column_size(b);
    END LOOP;
  ELSIF e = 'execute' THEN
    FOR v IN EXECUTE 'SELECT big FROM pt ORDER BY id' LOOP
      RETURN NEXT pg_column_size(v);
    END LOOP;
  ELSIF e = 'execute record' THEN
    FOR r IN EXECUTE 'SELECT big FROM pt ORDER BY id' LOOP
      RETURN NEXT pg_column_size(r.big);
    END LOOP;
  END IF;
END $$;
SELECT e, for_forms(e)
FROM unnest(ARRAY['scalar', 'short', 'record', 'targets', 'execute',
                  'execute record']) e;
-- A parameter holds the datum its caller's argument gives: a stored value
-- in its stored form (a short one packed), a value an expression formed
-- with a four-byte header. A binary-coercible conversion (varchar to text)
-- passes the datum on; a cast function forms a new one.
CREATE TABLE pv (id int, s text, big text, vc varchar(4000), c char(3));
INSERT INTO pv VALUES (1, 'short', repeat('a', 3000), repeat('b', 3000), 'ab');
CREATE FUNCTION psize(p text) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN pg_column_size(p); END $$;
CREATE FUNCTION vsize(p varchar) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN pg_column_size(p); END $$;
SELECT psize(s) AS s, psize(big) AS big, psize(s || '') AS formed,
       psize(big || '') AS formed_big, psize('lit') AS literal, psize(vc) AS vc,
       vsize(vc) AS vc_varchar, psize(c) AS bpchar, psize(id::text) AS cast
FROM pv;
CREATE FUNCTION pass_on(k int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE v text; w text; x text;
BEGIN
  SELECT s, big INTO v, w FROM pv WHERE id = k;
  x := upper('abc');
  RETURN psize(v) || ' ' || psize(w) || ' ' || psize(x) || ' ' || psize(v || '');
END $$;
SELECT pass_on(1);
CREATE PROCEDURE psize_proc(p text) LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'parameter %', pg_column_size(p); END $$;
CALL psize_proc('abc');
DO $$
DECLARE b text;
BEGIN
  SELECT big INTO b FROM pv WHERE id = 1;
  CALL psize_proc(b);
  CALL psize_proc(b || '');
END $$;
-- A value a query forms is held as its result tuple formed it, and a
-- FETCH reads a cursor's row as its query gave it.
CREATE FUNCTION formed_rows() RETURNS SETOF int LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR FOR SELECT s, big, big || '' AS f, s || '' AS sf FROM pv;
  a text; b text; f text; sf text; r record; v text; w text;
BEGIN
  OPEN c;
  FETCH c INTO a, b, f, sf;
  RETURN NEXT pg_column_size(a); RETURN NEXT pg_column_size(b);
  RETURN NEXT pg_column_size(f); RETURN NEXT pg_column_size(sf);
  CLOSE c;
  OPEN c; FETCH c INTO r; CLOSE c;
  RETURN NEXT pg_column_size(r.big); RETURN NEXT pg_column_size(r.f);
  FOR v IN SELECT big || '' FROM pv LOOP RETURN NEXT pg_column_size(v); END LOOP;
  SELECT big || '' INTO w FROM pv; RETURN NEXT pg_column_size(w);
  SELECT repeat('z', 200) INTO w; RETURN NEXT pg_column_size(w);
  FOR r IN SELECT big || '' AS b FROM pv LOOP RETURN NEXT pg_column_size(r.b); END LOOP;
  EXECUTE 'SELECT big FROM pv' INTO r; RETURN NEXT pg_column_size(r.big);
  EXECUTE 'SELECT big || '''' FROM pv' INTO w; RETURN NEXT pg_column_size(w);
END $$;
SELECT formed_rows();
