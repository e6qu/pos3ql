-- A function passes the datum it returns on unchanged: a PL/pgSQL
-- function's RETURN value as the function held it, a SQL function's
-- inlined body as its caller's argument, and an executed body's result as
-- its result tuple holds it (a short value packed, a stored one in its
-- TOAST form). An IMMUTABLE call of constants is folded into a new datum.
CREATE TABLE pv (id int, s text, big text, vc varchar(4000));
INSERT INTO pv VALUES (1, 'short', repeat('a', 3000), repeat('b', 3000));
CREATE FUNCTION ret_var(k int, e text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE v text;
BEGIN
  IF e = 'big' THEN SELECT big INTO v FROM pv WHERE id = k;
  ELSIF e = 's' THEN SELECT s INTO v FROM pv WHERE id = k;
  ELSIF e = 'formed' THEN v := upper('abc');
  ELSIF e = 'bigformed' THEN SELECT big || '' INTO v FROM pv WHERE id = k;
  END IF;
  RETURN v;
END $$;
SELECT e, pg_column_size(ret_var(1, e))
FROM unnest(ARRAY['big', 's', 'formed', 'bigformed']) e;
CREATE FUNCTION ret_param(p text) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN p; END $$;
CREATE FUNCTION ret_param_expr(p text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN RETURN p || ''; END $$;
CREATE FUNCTION ret_query(k int) RETURNS text LANGUAGE plpgsql AS $$
BEGIN RETURN (SELECT s FROM pv WHERE id = k); END $$;
CREATE FUNCTION ret_imm(p text) RETURNS text IMMUTABLE LANGUAGE plpgsql AS $$
BEGIN RETURN p; END $$;
SELECT pg_column_size(ret_param(s)) AS s, pg_column_size(ret_param(big)) AS big,
       pg_column_size(ret_param(s || '')) AS formed,
       pg_column_size(ret_param_expr(s)) AS expr, pg_column_size(ret_param(vc)) AS vc,
       pg_column_size(ret_query(1)) AS query, pg_column_size(ret_imm(s)) AS imm,
       pg_column_size(ret_imm('abc')) AS folded,
       pg_column_size(ret_param(ret_param(big))) AS nested
FROM pv;
CREATE FUNCTION sql_col(k int) RETURNS text LANGUAGE sql AS $$ SELECT big FROM pv WHERE id = k $$;
CREATE FUNCTION sql_s(k int) RETURNS text LANGUAGE sql AS $$ SELECT s FROM pv WHERE id = k $$;
CREATE FUNCTION sql_formed(k int) RETURNS text LANGUAGE sql AS $$
SELECT s || '' FROM pv WHERE id = k $$;
CREATE FUNCTION sql_id(p text) RETURNS text LANGUAGE sql AS $$ SELECT p $$;
CREATE FUNCTION sql_id_strict(p text) RETURNS text LANGUAGE sql STRICT AS $$ SELECT p $$;
CREATE FUNCTION sql_atomic(p text) RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT p; END;
CREATE FUNCTION sql_upper(p text) RETURNS text LANGUAGE sql AS $$ SELECT upper(p) $$;
SELECT pg_column_size(sql_col(1)) AS col, pg_column_size(sql_s(1)) AS s,
       pg_column_size(sql_formed(1)) AS formed, pg_column_size(sql_id(s)) AS id_s,
       pg_column_size(sql_id(big)) AS id_big, pg_column_size(sql_id_strict(big)) AS strict,
       pg_column_size(sql_atomic(s)) AS atomic, pg_column_size(sql_upper(s)) AS upper
FROM pv;
-- A parameter, a variable, a FROM item or a stored row given a function's
-- result keeps that datum.
CREATE FUNCTION psize(p text) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN pg_column_size(p); END $$;
SELECT psize(ret_param(s)) AS s, psize(ret_param(big)) AS big, psize(sql_col(1)) AS col,
       psize(ret_var(1, 's')) AS var
FROM pv;
DO $$
DECLARE v text;
BEGIN
  v := ret_var(1, 'big');
  RAISE NOTICE 'big %', pg_column_size(v);
  v := ret_var(1, 's');
  RAISE NOTICE 's %', pg_column_size(v);
  v := ret_param(upper('abc'));
  RAISE NOTICE 'formed %', pg_column_size(v);
END $$;
SELECT pg_column_size(r) AS big FROM pv, ret_param(pv.big) r;
SELECT pg_column_size(r) AS s FROM ret_var(1, 's') r;
CREATE TABLE dst (t text);
INSERT INTO dst SELECT ret_param(big) FROM pv;
INSERT INTO dst SELECT ret_var(1, 'big');
INSERT INTO dst SELECT sql_col(1);
INSERT INTO dst SELECT ret_param(s) FROM pv;
SELECT pg_column_size(t) FROM dst ORDER BY 1;
-- A user-defined cast, operator or aggregate support function, and a
-- domain's conversion, pass their argument's datum on as the
-- caller holds it; a procedure's output parameters come back as a row.
CREATE FUNCTION psize2(a text, b text) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN pg_column_size(a) * 10000 + pg_column_size(b); END $$;
CREATE CAST (text AS int4) WITH FUNCTION psize(text);
SELECT s::int4 AS s, big::int4 AS big, (s || '')::int4 AS formed FROM pv;
CREATE OPERATOR @@ (RIGHTARG = text, FUNCTION = psize);
CREATE OPERATOR @@@ (LEFTARG = text, RIGHTARG = text, FUNCTION = psize2);
SELECT @@ s AS s, @@ big AS big, @@ (s || '') AS formed, s @@@ big AS both FROM pv;
-- `@@@` is an operator of its own, built in for `tsvector` and `tsquery`.
SELECT to_tsvector('simple', 'a b') @@@ to_tsquery('simple', 'a') AS vq,
       to_tsquery('simple', 'c') @@@ to_tsvector('simple', 'a b') AS qv;
CREATE VIEW match_view AS
SELECT to_tsvector('simple', 'a b') @@@ to_tsquery('simple', 'a') AS m, s @@@ big AS u FROM pv;
SELECT pg_get_viewdef('match_view');
DROP VIEW match_view;
CREATE FUNCTION sizestep(acc int, v text) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN coalesce(acc, 0) * 100000 + pg_column_size(v); END $$;
CREATE AGGREGATE sizeagg(text) (SFUNC = sizestep, STYPE = int);
SELECT sizeagg(s) AS s, sizeagg(big) AS big, sizeagg(s || '') AS formed FROM pv;
CREATE DOMAIN dt AS text;
CREATE FUNCTION dsize(p dt) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN pg_column_size(p); END $$;
SELECT psize(s::dt) AS s, psize(big::dt) AS big, dsize(s::dt) AS ds, dsize(big::dt) AS dbig,
       dsize(s) AS implicit
FROM pv;
CREATE PROCEDURE outp(k int, INOUT a text, OUT b text) LANGUAGE plpgsql AS $$
BEGIN SELECT s, big INTO a, b FROM pv WHERE id = k; END $$;
CREATE PROCEDURE outq(k int, INOUT a text, OUT b text) LANGUAGE plpgsql AS $$
BEGIN a := upper('abc'); b := (SELECT big || '' FROM pv WHERE id = k); END $$;
DO $$
DECLARE a text; b text; r record;
BEGIN
  CALL outp(1, a, b);
  RAISE NOTICE 'outp % %', pg_column_size(a), pg_column_size(b);
  CALL outq(1, a, b);
  RAISE NOTICE 'outq % %', pg_column_size(a), pg_column_size(b);
  EXECUTE 'CALL outp(1, NULL, NULL)' INTO r;
  RAISE NOTICE 'execute % %', pg_column_size(r.a), pg_column_size(r.b);
END $$;
DROP TABLE pv, dst;
