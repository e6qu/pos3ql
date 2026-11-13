-- FOR loops over rows into a list of scalar variables: each row's columns
-- are assigned in order, missing ones NULL; a loop without rows sets them
-- NULL. Integer and cursor loops take one variable.
CREATE SCHEMA plpgsql_for_lists;
SET search_path = plpgsql_for_lists, public;
DO $$ DECLARE a int; b text; BEGIN FOR a, b IN SELECT 1, 'x' UNION ALL SELECT 2, 'y' LOOP RAISE NOTICE '% %', a, b; END LOOP; END $$;
DO $$ DECLARE a int; b text; BEGIN FOR a, b IN EXECUTE 'SELECT 1, ''x''' LOOP RAISE NOTICE '% %', a, b; END LOOP; END $$;
DO $$ DECLARE a int; b text; c CURSOR FOR SELECT 1, 'x'; BEGIN FOR a, b IN c LOOP RAISE NOTICE '% %', a, b; END LOOP; END $$;
DO $$ DECLARE a int; b int; BEGIN FOR a, b IN 1..3 LOOP NULL; END LOOP; END $$;
DO $$ DECLARE a int; b int; BEGIN FOR a, b IN SELECT 1 LOOP RAISE NOTICE '% %', a, b; END LOOP; END $$;
DO $$ DECLARE a int; b int; BEGIN FOR a, b IN SELECT 1, 2, 3 LOOP RAISE NOTICE '% %', a, b; END LOOP; END $$;
DO $$ DECLARE a int := 5; b int := 6; BEGIN FOR a, b IN SELECT 1, 2 WHERE false LOOP NULL; END LOOP; RAISE NOTICE '% % %', a, b, FOUND; END $$;
DO $$ DECLARE a int; r record; BEGIN FOR a, r IN SELECT 1, 2 LOOP NULL; END LOOP; END $$;
DO $$ DECLARE a int; b text; BEGIN FOR a, b IN SELECT '7', 8 LOOP RAISE NOTICE '% %', a, b; END LOOP; END $$;
DO $$ DECLARE a int; b int; BEGIN FOR a, nope IN SELECT 1, 2 LOOP NULL; END LOOP; END $$;
DO $$ DECLARE a int; b int; BEGIN FOREACH a, b IN ARRAY ARRAY[1,2] LOOP NULL; END LOOP; END $$;
CREATE TYPE pair AS (x int, y text);
DO $$ DECLARE a int; b text; BEGIN FOREACH a, b IN ARRAY ARRAY[ROW(1, 'p')::pair, ROW(2, 'q')::pair] LOOP RAISE NOTICE '% %', a, b; END LOOP; END $$;
CREATE TABLE ft (x int, y text);
INSERT INTO ft VALUES (1, 'a'), (2, 'b');
DO $$ DECLARE a int; b text; BEGIN FOR a, b IN UPDATE ft SET x = x + 10 RETURNING x, y LOOP RAISE NOTICE '% %', a, b; END LOOP; END $$;
DO $$ DECLARE a int; b int; BEGIN FOR a, b IN SELECT 1, 2 LOOP a := a + 1; END LOOP; RAISE NOTICE '% %', a, b; END $$;
DO $$ DECLARE a int; BEGIN FOR a IN SELECT 1, 2 LOOP RAISE NOTICE '%', a; END LOOP; END $$;
SET plpgsql.extra_errors = 'strict_multi_assignment';
DO $$ DECLARE a int; b int; BEGIN FOR a, b IN SELECT 1 LOOP NULL; END LOOP; END $$;
DO $$ DECLARE a int; BEGIN FOR a IN SELECT 1, 2 LOOP NULL; END LOOP; END $$;
DO $$ DECLARE a int; b int; BEGIN FOR a, b IN SELECT 1 WHERE false LOOP NULL; END LOOP; END $$;
RESET plpgsql.extra_errors;
DROP SCHEMA plpgsql_for_lists CASCADE;
