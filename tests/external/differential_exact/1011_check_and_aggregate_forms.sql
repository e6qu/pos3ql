-- A domain CHECK reads VALUE as the coercion holds it: the datum the cast
-- was given unless the base type's conversion formed a new one.
CREATE TABLE dv (id int, s text, big text);
INSERT INTO dv VALUES (1, 'short', repeat('a', 3000)), (2, 'tiny', repeat('b', 3000));
CREATE FUNCTION note_size(t text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'size %', pg_column_size(t); RETURN true; END $$;
CREATE DOMAIN dsz AS text CHECK (note_size(VALUE) AND pg_column_size(VALUE) > 0);
SELECT s::dsz IS NOT NULL AS s FROM dv WHERE id = 1;
SELECT big::dsz IS NOT NULL AS big FROM dv WHERE id = 1;
SELECT (s || '')::dsz IS NOT NULL AS formed FROM dv WHERE id = 1;
SELECT 'abc'::dsz IS NOT NULL AS literal;
SELECT CAST(big AS dsz) IS NOT NULL AS cast FROM dv WHERE id = 1;
CREATE DOMAIN dsz2 AS text CHECK (pg_column_size(VALUE) IN (6, 44));
SELECT s::dsz2 AS s, length(big::dsz2) AS big FROM dv WHERE id = 1;
SELECT (s || '')::dsz2 FROM dv WHERE id = 1;
-- An aggregate's transition function is passed its state as the previous
-- call returned it, its inputs as the query holds them (a sort or DISTINCT
-- passes each datum on), and its final function the state.
CREATE FUNCTION stepkeep(acc text, v text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'acc % v %', pg_column_size(acc), pg_column_size(v); RETURN v; END $$;
CREATE FUNCTION finsize(acc text) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN pg_column_size(acc); END $$;
CREATE AGGREGATE keepagg(text) (SFUNC = stepkeep, STYPE = text, FINALFUNC = finsize);
CREATE AGGREGATE keepinit(text) (SFUNC = stepkeep, STYPE = text, INITCOND = 'x');
CREATE FUNCTION stepstrict(acc text, v text) RETURNS text STRICT LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'strict acc % v %', pg_column_size(acc), pg_column_size(v); RETURN acc; END $$;
CREATE AGGREGATE keepfirst(text) (SFUNC = stepstrict, STYPE = text, FINALFUNC = finsize);
SELECT keepagg(s) FROM dv;
SELECT keepagg(big) FROM dv;
SELECT keepagg(s ORDER BY id) FROM dv;
SELECT keepagg(big ORDER BY id DESC) FROM dv;
SELECT keepagg(DISTINCT s) FROM dv;
SELECT keepagg(DISTINCT big) FROM dv;
SELECT keepagg(s || '') FROM dv;
SELECT keepinit(s) FROM dv WHERE id = 1;
SELECT keepfirst(big) FROM dv;
SELECT keepfirst(s ORDER BY id) FROM dv;
-- An array constructor's element reaches its cast as the query holds it.
CREATE FUNCTION psz(p text) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN pg_column_size(p); END $$;
CREATE CAST (text AS int4) WITH FUNCTION psz(text);
SELECT ARRAY[s, big]::int4[] AS elements FROM dv ORDER BY id;
SELECT ARRAY[s || '', upper(s)]::int4[] AS formed FROM dv WHERE id = 1;
SELECT CAST(ARRAY[big] AS int4[]) AS cast FROM dv WHERE id = 1;
DROP CAST (text AS int4);
DROP TABLE dv;
DROP DOMAIN dsz, dsz2;
DROP AGGREGATE keepagg(text), keepinit(text), keepfirst(text);
DROP FUNCTION note_size, stepkeep, finsize, stepstrict, psz;
