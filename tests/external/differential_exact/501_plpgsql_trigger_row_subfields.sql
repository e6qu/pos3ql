-- A trigger function assigns sub-fields of NEW's and OLD's composite columns
-- (NEW.col.f, NEW.col.f.g), checked as PostgreSQL plans the assignment.
CREATE TYPE ts_pair AS (a int, b int);
CREATE TYPE ts_outer AS (p ts_pair, c int);
CREATE TABLE ts_t (id int, r ts_pair, o ts_outer);
CREATE FUNCTION ts_f() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.r.a := 10;
  NEW.o.p.b := 20;
  NEW.o.c := (NEW.o).c + 1;
  RETURN NEW;
END $$;
CREATE TRIGGER ts_trig BEFORE INSERT ON ts_t FOR EACH ROW EXECUTE FUNCTION ts_f();
INSERT INTO ts_t VALUES (1, ROW(1, 2), ROW(ROW(3, 4), 5));
SELECT * FROM ts_t;
CREATE FUNCTION ts_g() RETURNS trigger LANGUAGE plpgsql AS $$
<<blk>>
BEGIN
  NEW.r.zz := 1;
  RETURN NEW;
END $$;
CREATE TRIGGER ts_trig2 BEFORE INSERT ON ts_t FOR EACH ROW EXECUTE FUNCTION ts_g();
INSERT INTO ts_t VALUES (2, ROW(1, 2), ROW(ROW(3, 4), 5));
CREATE FUNCTION ts_h() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  OLD.r.a := 1;
  NEW.id.x := 1;
  RETURN NEW;
END $$;
CREATE TRIGGER ts_trig3 BEFORE UPDATE ON ts_t FOR EACH ROW EXECUTE FUNCTION ts_h();
DROP TRIGGER ts_trig2 ON ts_t;
UPDATE ts_t SET id = 3;
SELECT * FROM ts_t;
DROP TABLE ts_t;
DROP FUNCTION ts_f(), ts_g(), ts_h();
DROP TYPE ts_outer, ts_pair;
