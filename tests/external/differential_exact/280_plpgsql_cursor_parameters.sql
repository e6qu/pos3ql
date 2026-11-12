-- A routine parameter used as a cursor variable must be a refcursor; the
-- check runs when the routine is created, as PostgreSQL's does.
CREATE FUNCTION fc280_1(c int) returns void language plpgsql as $$ begin close c; end $$;
CREATE FUNCTION fc280_2(c text) returns void language plpgsql as $$ declare r int; begin fetch c into r; end $$;
CREATE FUNCTION fc280_3(c refcursor) returns int language plpgsql as $$ declare r int; begin open c for select 1; fetch c into r; close c; return r; end $$;
SELECT fc280_3('x');
CREATE FUNCTION fc280_4(c int[]) returns void language plpgsql as $$ begin move c; end $$;
CREATE FUNCTION fc280_5(out c int) language plpgsql as $$ begin close c; end $$;
CREATE FUNCTION fc280_6(c int) returns void language plpgsql as $$ begin open c for select 1; end $$;
