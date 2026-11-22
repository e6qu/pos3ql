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
