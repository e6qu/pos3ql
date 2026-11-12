-- A PL/pgSQL cursor variable may be qualified by its block label in OPEN,
-- FETCH, MOVE and CLOSE.
DO $$ <<b>> DECLARE c refcursor; r int; BEGIN OPEN c FOR SELECT 1; FETCH c INTO r; CLOSE b.c; RAISE NOTICE 'closed %', r; END $$;
DO $$ <<b>> DECLARE c refcursor; BEGIN OPEN c FOR SELECT 1; CLOSE b.zz; END $$;
DO $$ <<b>> DECLARE c refcursor; BEGIN OPEN c FOR SELECT 1; CLOSE x.c; END $$;
DO $$ <<b>> DECLARE c int; BEGIN CLOSE b.c; END $$;
DO $$ <<b>> DECLARE c refcursor; BEGIN OPEN b.c FOR SELECT 1; CLOSE c; END $$;
DO $$ <<b>> DECLARE c refcursor; r int; BEGIN OPEN c FOR SELECT 1; FETCH b.c INTO r; RAISE NOTICE 'r %', r; END $$;
DO $$ <<b>> DECLARE c refcursor; r int; BEGIN OPEN c FOR SELECT 1; FETCH FROM b.c INTO r; MOVE b.c; RAISE NOTICE 'r %', r; END $$;
DO $$ <<outer>> DECLARE c refcursor; BEGIN
  DECLARE c refcursor; r int; BEGIN
    OPEN outer.c FOR SELECT 2;
    FETCH outer.c INTO r;
    RAISE NOTICE 'outer %', r;
    CLOSE outer.c;
  END;
END $$;
