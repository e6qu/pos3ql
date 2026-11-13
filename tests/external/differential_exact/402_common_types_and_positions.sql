-- COALESCE, GREATEST and LEAST take PostgreSQL's common type of their
-- arguments, failing for arguments of different categories or without an
-- implicit conversion (two tables' row types among them); a table's row
-- type is a cast target for a row constructor. Then analysis errors at a
-- literal operand, a field selection and a rule condition's OLD or NEW.
CREATE SCHEMA common_types;
SET search_path = common_types, public;
CREATE TABLE a (x int, y text);
CREATE TABLE b (x int, y text);
INSERT INTO a VALUES (1, 'p');
INSERT INTO b VALUES (2, 'q');
SELECT COALESCE(a, b) FROM a, b;
SELECT COALESCE(a, b) FROM a, b WHERE false;
SELECT COALESCE(a, a) FROM a;
SELECT COALESCE(a, ROW(1, 'x')) FROM a;
SELECT COALESCE(ROW(1, 'x'), a) FROM a;
SELECT ROW(1, 'q')::a, (ROW(2, 'r')::a).y;
SELECT COALESCE(1, 'x'::text);
SELECT COALESCE(1, true);
SELECT a.x FROM a WHERE COALESCE(1, true) = 1;
SELECT GREATEST(1, true);
SELECT LEAST(1, 'x'::text);
SELECT pg_typeof(COALESCE(1, 2.5)), pg_typeof(COALESCE(NULL, 1, 2.5)), COALESCE(NULL::int, 2.5);
SELECT pg_typeof(GREATEST(1, 2.5)), pg_typeof(COALESCE(1::int2, 2::int8)), COALESCE(1::int2, 2::int8);
SELECT COALESCE(1, 2.5), COALESCE(NULL, 'x'), pg_typeof(COALESCE(NULL, NULL));
CREATE DOMAIN sd AS varchar(10);
SELECT pg_typeof(COALESCE('x'::sd, NULL)), pg_typeof(COALESCE('x'::sd, 'y'::sd)), pg_typeof(COALESCE(NULL, 'x'::sd));
SELECT COALESCE('a'::varchar, 'b'::text), pg_typeof(COALESCE('a'::varchar, 'b'::text));
SELECT pg_typeof(COALESCE(current_date, now()));
SELECT CASE WHEN true THEN 1 ELSE true END FROM a WHERE false;
SELECT CASE WHEN true THEN 1 WHEN false THEN 'x'::text END;
SELECT a::a FROM a;
SELECT 1 + 'x';
SELECT 1 + 'x' FROM a;
SELECT 'x' + 1;
SELECT 1 < 'x';
SELECT 1 + '99999999999';
SELECT 1 / '0';
SELECT (1).x;
SELECT (a.x).y FROM a;
SELECT ('a'::text).x;
SELECT (ROW(1, 2)).x;
DO $$ DECLARE v int := 1; BEGIN v := v + 'x'; END $$;
DO $$ DECLARE v int := 1; BEGIN v := (v).x; END $$;
DO $$ DECLARE r record; BEGIN SELECT 1 AS a INTO r; RAISE NOTICE '%', r.nope; END $$;
CREATE TABLE rt (n int);
CREATE RULE r1 AS ON INSERT TO rt WHERE OLD.n > 0 DO NOTHING;
CREATE RULE r2 AS ON DELETE TO rt WHERE NEW.n > 0 DO NOTHING;
DO $$ BEGIN CREATE RULE r3 AS ON INSERT TO rt WHERE OLD.n > 0 DO NOTHING; END $$;
DO $$ BEGIN EXECUTE 'CREATE RULE r4 AS ON INSERT TO rt WHERE OLD.n > 0 DO NOTHING'; END $$;
DROP SCHEMA common_types CASCADE;
