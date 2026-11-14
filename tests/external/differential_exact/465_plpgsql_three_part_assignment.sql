-- PL/pgSQL assigns block.record.field: a field of the record variable that
-- the labelled block declares, even where an inner block shadows it.
CREATE TYPE pt AS (a int, b int);
CREATE TABLE tt (a int, b int);
DO $$ <<blk>> DECLARE r pt; BEGIN blk.r.a := 1; RAISE NOTICE '%', r; END $$;
DO $$ <<blk>> DECLARE r tt; BEGIN blk.r.b := 2; blk.r.a := blk.r.b + 1; RAISE NOTICE '%', r; END $$;
DO $$ <<blk>> DECLARE r record; BEGIN SELECT 1 AS a, 2 AS b INTO r; blk.r.a := 5; RAISE NOTICE '%', r; END $$;
DO $$ DECLARE x int; BEGIN x.y.z := 1; END $$;
DO $$ <<blk>> DECLARE x int; BEGIN blk.x.z := 1; END $$;
DO $$ <<blk>> DECLARE r pt; BEGIN blk.r.c := 1; END $$;
CREATE FUNCTION f3() RETURNS pt LANGUAGE plpgsql AS $$ <<b1>> DECLARE r pt; BEGIN <<b2>> DECLARE r int := 3; BEGIN b1.r.a := r; b1.r.b := b2.r * 2; END; RETURN r; END $$;
SELECT f3();
DO $$ <<blk>> DECLARE r pt; c CONSTANT pt := ROW(1,2); BEGIN blk.c.a := 1; END $$;
DO $$ <<blk>> DECLARE r pt; BEGIN blk.r.a := 7; RAISE NOTICE '% %', blk.r.a, r.a; blk.r := ROW(1, 2); blk.r.b := blk.r.a + 10; RAISE NOTICE '%', r; END $$;
DO $$ <<blk>> DECLARE r pt; BEGIN FOR i IN 1..2 LOOP blk.r.a := i; END LOOP; RAISE NOTICE '%', r; END $$;
CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ <<b>> DECLARE r tt; BEGIN b.r.a := NEW.a; NEW.b := b.r.a * 10; RETURN NEW; END $$;
CREATE TRIGGER t BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION trg();
INSERT INTO tt VALUES (4, 0) RETURNING *;
