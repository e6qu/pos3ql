-- PL/pgSQL block and loop labels, labelled EXIT and CONTINUE, END labels,
-- variables qualified by a block, loop or routine label, and nested DECLARE
-- sections with their scoping, shadowing and initialization order.
CREATE FUNCTION lb85_nested() RETURNS text LANGUAGE plpgsql AS $$
<<blo>>
DECLARE x int := 1;
BEGIN
  <<bli>>
  DECLARE x int := x + 10; y int := x;
  BEGIN
    RETURN blo.x || ',' || bli.x || ',' || x || ',' || y;
  END bli;
END blo $$;
SELECT lb85_nested();
CREATE FUNCTION lb85_exit_block() RETURNS int LANGUAGE plpgsql AS $$
DECLARE n int := 0;
BEGIN
  <<blk>>
  BEGIN
    n := 1;
    EXIT blk;
    n := 2;
  END blk;
  RETURN n;
END $$;
SELECT lb85_exit_block();
CREATE FUNCTION lb85_loops() RETURNS int LANGUAGE plpgsql AS $$
DECLARE n int := 0;
BEGIN
  <<o>> FOR i IN 1..3 LOOP
    <<inr>> FOR j IN 1..3 LOOP
      n := n + o.i * 10 + inr.j;
      CONTINUE o WHEN j = 2;
      EXIT o WHEN i = 3;
    END LOOP inr;
  END LOOP o;
  RETURN n;
END $$;
SELECT lb85_loops();
CREATE FUNCTION lb85_counter_shadow() RETURNS text LANGUAGE plpgsql AS $$
<<b>> DECLARE i int := 7; total int := 0;
BEGIN
  <<lp>> FOR i IN 1..2 LOOP
    b.i := b.i + lp.i;
    total := total * 10 + i;
  END LOOP;
  RETURN i || ',' || total;
END $$;
SELECT lb85_counter_shadow();
CREATE FUNCTION lb85_routine_label(p int) RETURNS text LANGUAGE plpgsql AS $$
<<lb85_routine_label>> DECLARE p int := 5;
BEGIN
  PERFORM 1;
  RETURN lb85_routine_label.p || ',' || lb85_routine_label.found;
END $$;
SELECT lb85_routine_label(1);
CREATE FUNCTION lb85_parameter_label(a int) RETURNS int LANGUAGE plpgsql AS $$
<<q>> DECLARE b int := 2;
BEGIN
  RETURN lb85_parameter_label.a + q.b;
END $$;
SELECT lb85_parameter_label(1);
CREATE FUNCTION lb85_local_not_routine(a int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE b int := 2;
BEGIN
  RETURN lb85_local_not_routine.a + lb85_local_not_routine.b;
END $$;
SELECT lb85_local_not_routine(1);
CREATE FUNCTION lb85_targets() RETURNS text LANGUAGE plpgsql AS $$
<<b>> DECLARE x int; r record; arr int[] := '{1,2}';
BEGIN
  SELECT 5 INTO b.x;
  b.arr[1] := 9;
  FOR b.r IN SELECT 4 AS a LOOP
    x := x + b.r.a;
  END LOOP;
  RETURN x || ',' || arr::text || ',' || r.a;
END $$;
SELECT lb85_targets();
CREATE TABLE lb85 (v int);
INSERT INTO lb85 VALUES (10);
CREATE FUNCTION lb85_in_query() RETURNS int LANGUAGE plpgsql AS $$
<<a>> DECLARE v int := 5;
BEGIN
  UPDATE lb85 SET v = lb85.v + a.v WHERE lb85.v = 10;
  RETURN (SELECT a.v + lb85.v FROM lb85);
END $$;
SELECT lb85_in_query();
CREATE FUNCTION lb85_handler_exit() RETURNS text LANGUAGE plpgsql AS $$
DECLARE t text := '';
BEGIN
  <<b>> BEGIN
    INSERT INTO lb85 VALUES (1);
    EXIT b;
  EXCEPTION WHEN others THEN t := 'caught';
  END;
  <<c>> BEGIN
    RAISE EXCEPTION 'x';
  EXCEPTION WHEN others THEN
    t := t || 'handled';
    EXIT c;
  END c;
  RETURN t || (SELECT count(*) FROM lb85);
END $$;
SELECT lb85_handler_exit();
CREATE FUNCTION lb85_scope_loop() RETURNS text LANGUAGE plpgsql AS $$
DECLARE t text := '';
BEGIN
  FOR i IN 1..3 LOOP
    DECLARE s int := i * 2;
    BEGIN
      t := t || s;
      s := 0;
    END;
  END LOOP;
  <<w>> WHILE length(t) < 10 LOOP
    t := t || '.';
    <<blk>> BEGIN EXIT w WHEN length(t) = 5; END;
  END LOOP;
  RETURN t;
END $$;
SELECT lb85_scope_loop();
CREATE FUNCTION lb85_init_error() RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  DECLARE z int := 1/0;
  BEGIN
    RETURN 'unreached';
  EXCEPTION WHEN others THEN RETURN 'inner';
  END;
EXCEPTION WHEN others THEN RETURN 'outer ' || SQLERRM;
END $$;
SELECT lb85_init_error();
CREATE FUNCTION lb85_types() RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  DECLARE v text := 'a';
  BEGIN
    DECLARE v int := 3;
    BEGIN
      v := v + 1;
    END;
    RETURN v || 'b';
  END;
END $$;
SELECT lb85_types();
DO $$ <<d>> DECLARE v int := 3; BEGIN EXIT d; RAISE EXCEPTION 'unreached'; END $$;
DO $$ BEGIN DECLARE BEGIN NULL; END; END $$;
DO $$ <<o>> BEGIN <<o>> LOOP EXIT o; END LOOP; END $$;
-- Rejected labels, END labels, EXIT and CONTINUE targets and declarations.
DO $$ BEGIN <<b>> BEGIN CONTINUE b; END; END $$;
DO $$ BEGIN BEGIN EXIT; END; END $$;
DO $$ BEGIN CONTINUE; END $$;
DO $$ BEGIN EXIT nolabel; END $$;
DO $$ <<a>> BEGIN NULL; END b $$;
DO $$ BEGIN NULL; END b $$;
DO $$ BEGIN <<lp>> LOOP EXIT lp; END LOOP wrong; END $$;
DO $$ BEGIN <<x>> NULL; END $$;
DO $$ BEGIN <<x>> IF true THEN NULL; END IF; END $$;
DO $$ DECLARE <<x>> BEGIN NULL; END $$;
DO $$ BEGIN DECLARE <<x>> BEGIN NULL; END; END $$;
DO $$ DECLARE a int; a int; BEGIN NULL; END $$;
DO $$ BEGIN DECLARE a int; a int; BEGIN NULL; END; END $$;
DO $$ BEGIN DECLARE a int NOT NULL; BEGIN NULL; END; END $$;
DO $$ BEGIN DECLARE a lb85_no_such_type; BEGIN NULL; END; END $$;
DO $$ BEGIN DECLARE c CONSTANT int := 1; BEGIN c := 2; END; END $$;
DO $$ <<b>> DECLARE x int; BEGIN RAISE NOTICE '%', b.nope; END $$;
DO $$ <<b>> DECLARE x int := 1; BEGIN b.nope := 3; END $$;
DO $$ BEGIN DECLARE z int := 1/0; BEGIN NULL; END; END $$;
