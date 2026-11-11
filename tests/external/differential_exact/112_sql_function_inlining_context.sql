-- PostgreSQL runs an IMMUTABLE SQL function called with constant arguments
-- at plan time (`statement 1`), folds the inlined body of another call with
-- constants (`during inlining`), and reports no frame for an inlined call
-- with varying arguments.
CREATE FUNCTION il112_i(x int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 10 / x';
CREATE FUNCTION il112_v(x int) RETURNS int LANGUAGE sql VOLATILE AS 'SELECT 10 / x';
CREATE FUNCTION il112_s(x int) RETURNS int LANGUAGE sql STABLE AS 'SELECT 10 / x';
CREATE FUNCTION il112_m(x int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 1; SELECT 10 / x';
CREATE TABLE il112 (a int);
INSERT INTO il112 VALUES (0);
SELECT il112_i(0);
SELECT il112_v(0);
SELECT il112_s(0);
SELECT il112_m(0);
SELECT il112_i(a) FROM il112;
SELECT il112_v(a) FROM il112;
SELECT il112_i(0) FROM il112 WHERE false;
SELECT il112_v(0) FROM il112 WHERE false;
SELECT il112_i(1 - 1);
SELECT il112_v(1) + il112_v(0);
SELECT 1 + il112_i(0);
CREATE FUNCTION il112_w(x int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 10 / x FROM il112';
SELECT il112_w(0);
SELECT il112_w(a) FROM il112;
PREPARE il112_pi(int) AS SELECT il112_i($1);
EXECUTE il112_pi(0);
PREPARE il112_pv(int) AS SELECT il112_v($1);
EXECUTE il112_pv(0);
CREATE FUNCTION il112_pl(v int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN il112_i(v); END $$;
SELECT il112_pl(0);
-- A volatile call inside the inlined body runs at execution, not inlining.
CREATE FUNCTION il112_inner(x int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 10 / x; END $$;
CREATE FUNCTION il112_outer(a int) RETURNS int LANGUAGE sql AS 'SELECT il112_inner(a)';
SELECT il112_outer(0);
