-- An untyped literal argument of a function in FROM converts to its
-- parameter's type while the call is analysed: its input fails at the
-- literal, in positional and named notation, and a built-in's literal
-- arguments take the selected overload's types.
CREATE FUNCTION fa831_rows(a integer, b text DEFAULT 'q') RETURNS TABLE (x integer, y text)
  LANGUAGE sql AS 'SELECT a, b';
CREATE FUNCTION fa831_add(a integer, b integer DEFAULT 2) RETURNS integer
  LANGUAGE sql AS 'SELECT a + b';
CREATE FUNCTION fa831_each(a integer) RETURNS SETOF integer
  LANGUAGE plpgsql AS $$BEGIN RETURN NEXT a; END$$;
SELECT * FROM fa831_rows(a => 'x');
SELECT * FROM fa831_rows(b => 'q', a => 'x');
SELECT * FROM fa831_rows(1, b => 'z') r, fa831_rows(a => 'y') s;
SELECT * FROM fa831_add('x');
SELECT * FROM fa831_add(a => 'x');
SELECT * FROM fa831_add(b => 'w', a => 1);
SELECT * FROM fa831_add(1, b => 'x');
SELECT * FROM fa831_add(a => 1, b => '1.5') AS t;
SELECT * FROM fa831_each(a => 'v');
SELECT * FROM fa831_rows(a := 'x');
SELECT * FROM ROWS FROM (fa831_rows(a => 'x'));
SELECT * FROM LATERAL fa831_rows(a => 'x');
SELECT * FROM fa831_add(a => '4', b => '5');
-- Built-in set-returning functions in FROM.
SELECT * FROM generate_series(1, '3');
SELECT * FROM generate_series(1, 3, '1');
SELECT * FROM generate_series(1, 'x');
SELECT * FROM generate_series(1.5, '3');
SELECT * FROM generate_subscripts('{1,2}'::integer[], '1');
SELECT * FROM generate_series(1, '3') g CROSS JOIN generate_series(1, '2') h;
SELECT * FROM ROWS FROM (generate_series(1, '2'), generate_series(1, '3'));
SELECT g FROM generate_series(1, '3') g WHERE g > 1;
BEGIN;
DECLARE fa831_cursor CURSOR FOR SELECT * FROM generate_series(1, '3');
FETCH ALL fa831_cursor;
COMMIT;
BEGIN;
DECLARE fa831_bad CURSOR FOR SELECT * FROM generate_series(1, 'x');
ROLLBACK;
