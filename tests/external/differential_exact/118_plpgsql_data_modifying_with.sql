-- PL/pgSQL SELECT ... INTO and RETURN QUERY run data-modifying WITH queries
-- once; the main query reads the tables as they were before those writes,
-- and a WITH query nothing reads still runs.
CREATE TABLE w118 (id int, v text);
INSERT INTO w118 VALUES (1, 'a');
CREATE FUNCTION removed118() RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY WITH d AS (DELETE FROM w118 WHERE id > 2 RETURNING id) SELECT id FROM d ORDER BY id; END $$;
DO $$ DECLARE n int; m int; BEGIN WITH x AS (INSERT INTO w118 VALUES (2, 'b'), (3, 'c'), (4, 'd') RETURNING id) SELECT count(*), (SELECT count(*) FROM w118) INTO n, m FROM x; RAISE NOTICE 'n=% m=%', n, m; END $$;
DO $$ DECLARE n int; BEGIN WITH x AS (DELETE FROM w118 WHERE id = 4 RETURNING id), y AS (UPDATE w118 SET v = 'u' WHERE id = 1 RETURNING 1) SELECT max(id) INTO n FROM x; RAISE NOTICE 'n=%', n; END $$;
DO $$ DECLARE n int; BEGIN WITH x AS (DELETE FROM w118 WHERE id = 99 RETURNING id) SELECT id INTO STRICT n FROM x; END $$;
SELECT * FROM removed118();
SELECT * FROM w118 ORDER BY id;
DROP FUNCTION removed118();
DROP TABLE w118;
