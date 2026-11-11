-- A table's name declares a PL/pgSQL row variable, as `%ROWTYPE` does. Rows
-- move into row variables by position: missing fields become NULL, extra ones
-- are ignored and each field converts like an assignment, so one composite
-- column assigned to a row reaches its first field. `r.f := v` sets a field.
CREATE TABLE orders116(id int, item text, qty numeric(6,2));
INSERT INTO orders116 VALUES (1, 'apple', 3), (2, 'pear', 5);
CREATE TYPE pair116 AS (x int, y text);
CREATE TYPE text_pair116 AS (x text, y int);
DO $$ DECLARE r orders116; BEGIN SELECT * INTO r FROM orders116 WHERE id = 1; RAISE NOTICE '% % %', r.id, r.item, r; END $$;
DO $$ DECLARE r orders116%ROWTYPE; BEGIN SELECT * INTO r FROM orders116 WHERE id = 2; RAISE NOTICE '% %', r.item, r.qty; END $$;
DO $$ DECLARE r public.orders116%ROWTYPE; BEGIN r.id := 7; r.item := 'kiwi'; r.qty := '2.555'; RAISE NOTICE '%', r; r.id := r.id + 1; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE r orders116; BEGIN RAISE NOTICE '% % %', r, r IS NULL, r.id; r := ROW(9, 'fig', 1.5); RAISE NOTICE '%', r; END $$;
DO $$ DECLARE r orders116; BEGIN SELECT o INTO r FROM orders116 o WHERE id = 1; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE p pair116; BEGIN SELECT ROW(1,'a')::pair116 INTO p; RAISE NOTICE '%', p; END $$;
DO $$ DECLARE p pair116; BEGIN SELECT 3, 'c' INTO p; RAISE NOTICE '%', p; END $$;
DO $$ DECLARE p pair116; BEGIN SELECT 1, 'a', 3 INTO p; RAISE NOTICE '%', p; SELECT 1 INTO p; RAISE NOTICE '%', p; END $$;
DO $$ DECLARE p text_pair116; BEGIN SELECT ROW(1,2)::pair116 INTO p; RAISE NOTICE '%', p; END $$;
DO $$ DECLARE p text_pair116; r orders116; BEGIN SELECT '5', 7.9 INTO p; RAISE NOTICE '%', p; SELECT 1.5, 2 INTO r; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE r record; BEGIN SELECT o INTO r FROM orders116 o WHERE id = 1; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE p text_pair116; BEGIN p := ROW(1,2)::pair116; RAISE NOTICE '%', p; p := (SELECT ROW(3,4)::pair116); RAISE NOTICE '%', p; END $$;
DO $$ DECLARE p pair116; BEGIN p := ROW(1); RAISE NOTICE '%', p; p := ROW(1,'a',3); RAISE NOTICE '%', p; p := ROW('7', 8); RAISE NOTICE '%', p; END $$;
DO $$ DECLARE p pair116; BEGIN p := 5; END $$;
DO $$ DECLARE p pair116; BEGIN p := '(3,c)'; RAISE NOTICE '%', p; END $$;
DO $$ DECLARE r orders116; BEGIN FOR r IN SELECT 1, 'a' LOOP RAISE NOTICE '%', r; END LOOP; END $$;
DO $$ DECLARE r orders116; BEGIN FOR r IN SELECT * FROM orders116 ORDER BY id LOOP RAISE NOTICE '%', r.item; END LOOP; END $$;
DO $$ DECLARE r orders116; c refcursor; BEGIN OPEN c FOR SELECT 4; FETCH c INTO r; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE r orders116; BEGIN INSERT INTO orders116 VALUES (9,'z',1) RETURNING id INTO r; RAISE NOTICE '%', r; DELETE FROM orders116 WHERE id = 9; END $$;
DO $$ DECLARE r orders116; BEGIN SELECT * INTO r FROM orders116 WHERE id = 1; UPDATE orders116 SET qty = r.qty + 1 WHERE id = r.id; END $$;
DO $$ DECLARE r orders116; BEGIN SELECT * INTO STRICT r FROM orders116 WHERE id = 99; END $$;
DO $$ DECLARE r orders116 := ROW(5,'z',2); BEGIN RAISE NOTICE '%', r; END $$;
DO $$ DECLARE r orders116%ROWTYPE; BEGIN r.nope := 1; END $$;
DO $$ DECLARE r orders116; BEGIN r.id := 'abc'; END $$;
DO $$ DECLARE p pair116; BEGIN p.x := '12'; p.y := 3; RAISE NOTICE '% %', p, p.y || 'z'; END $$;
DO $$ DECLARE p record; BEGIN p.x := 1; END $$;
DO $$ DECLARE p record; BEGIN SELECT 1 AS x, 'a'::text AS y INTO p; p.x := '42'; p.y := 5; RAISE NOTICE '%', p; p.x := 'bad'; END $$;
DO $$ <<b>> DECLARE r orders116; id int; BEGIN b.id := 5; RAISE NOTICE '% %', id, r.id; END $$;
SELECT * FROM orders116 ORDER BY id;
-- Record literals follow record_in: an empty field is NULL, quotes and
-- backslashes protect text, and the field count must match.
SELECT '(3,c)'::pair116, ' ( 3 , "c d" ) '::pair116, '(,)'::pair116, '(1,"")'::pair116, '(1,a"b,c"d)'::pair116, '(1,a\,b)'::pair116;
SELECT '5'::pair116;
SELECT '(3'::pair116;
SELECT '(3,c)x'::pair116;
SELECT '(1,2,3)'::pair116;
SELECT '(1)'::pair116;
SELECT '(1,"abc'::pair116;
SELECT 'x'::text::pair116;
SELECT 5::pair116;
SELECT ROW(1, 'a,b"c')::pair116::text::pair116;
