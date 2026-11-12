-- PL/pgSQL assigns a row to a list of INTO variables position by position:
-- extra columns are ignored and variables without a column become NULL, for
-- SELECT INTO, EXECUTE INTO and FETCH INTO alike.
DO $$
DECLARE x int := 9; y int := 9;
BEGIN
  SELECT 1, 2 INTO x;
  RAISE NOTICE 'select % %', x, y;
  SELECT 5 INTO x, y;
  RAISE NOTICE 'select % %', x, y;
  EXECUTE 'SELECT 3, 4' INTO x;
  RAISE NOTICE 'execute % %', x, y;
  y := 9;
  EXECUTE 'SELECT 6' INTO x, y;
  RAISE NOTICE 'execute % %', x, y;
END $$;
CREATE FUNCTION into_counts() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  wide CURSOR FOR SELECT 7 AS a, 8 AS b;
  narrow CURSOR FOR SELECT 1 AS a;
  x int := 0;
  y int := 0;
BEGIN
  OPEN wide; FETCH wide INTO x; CLOSE wide;
  OPEN narrow; FETCH narrow INTO x, y; CLOSE narrow;
  SELECT 1, 2 INTO STRICT y;
  RETURN x || ':' || coalesce(y::text, 'null');
END $$;
SELECT into_counts();
DROP ROUTINE into_counts() CASCADE;
