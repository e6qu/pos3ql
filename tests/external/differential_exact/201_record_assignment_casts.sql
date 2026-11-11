-- A named composite value assigns to a PL/pgSQL record and casts to record.
CREATE TABLE ra200(id int, v int);
DO $$DECLARE rec record; BEGIN rec := ROW(1,2)::ra200; RAISE NOTICE '% %', rec, rec.id; END$$;
SELECT ROW(1,2)::ra200::record;
DROP TABLE ra200;
