-- A domain's base type modifier follows the coercion context: an explicit
-- cast truncates, an assignment, implicit coercion or input rejects. The
-- planner folds a constant's length coercion, so it fails without a cursor
-- and before any row is read; an input function's failure points at the
-- literal.
CREATE DOMAIN dl830_short AS varchar(3);
CREATE DOMAIN dl830_inner AS dl830_short;
CREATE DOMAIN dl830_pair AS char(2);
CREATE DOMAIN dl830_num AS numeric(4,1);
CREATE DOMAIN dl830_bits AS bit(3);
CREATE DOMAIN dl830_vbits AS varbit(3);
CREATE DOMAIN dl830_pos AS integer CHECK (VALUE > 0);
CREATE TYPE dl830_row AS (x dl830_short);
CREATE TABLE dl830 (a dl830_short, b dl830_short[], n dl830_num, p dl830_pos[]);
CREATE TABLE dl830_empty (a integer, b varchar(3), c dl830_short, d dl830_pos);
-- Explicit casts truncate.
SELECT 'abcdef'::dl830_short, CAST('abcdef' AS dl830_short);
SELECT 'abcdef'::varchar(10)::dl830_short, 'abcdef'::text::dl830_inner;
SELECT 'abcd'::dl830_pair, 'abcd'::text::dl830_pair;
SELECT B'10101'::dl830_bits, B'10101'::varbit::dl830_bits, B'10101'::dl830_vbits;
SELECT ARRAY['abcdef']::dl830_short[], ARRAY[ARRAY['abcdef'::text]]::dl830_short[];
SELECT '{abcdef}'::text[]::dl830_short[];
SELECT ROW('abcdef')::dl830_row, ROW('abcdef'::text)::dl830_row;
SELECT x::dl830_short FROM (VALUES ('abcdef'), ('ghijkl')) v(x);
SELECT array_fill('abcdef'::dl830_short, ARRAY[1]), 'abcdef'::text::dl830_short || 'x';
-- A numeric domain still overflows, unpositioned.
SELECT 123.456::dl830_num;
SELECT '12345'::dl830_num;
-- Assignments reject.
INSERT INTO dl830 (a) VALUES ('abcdef');
INSERT INTO dl830 (a) VALUES ('abcdef'::varchar);
INSERT INTO dl830 (a) VALUES ('ab'), ('abcdef');
INSERT INTO dl830 (a) VALUES ('abc   ');
INSERT INTO dl830 (b) VALUES (ARRAY['abcdef']);
INSERT INTO dl830 (b) VALUES (ARRAY['abcdef'::dl830_short]);
SELECT a, b FROM dl830 ORDER BY a;
-- Function arguments and results coerce implicitly.
CREATE FUNCTION dl830_echo(x dl830_short) RETURNS text LANGUAGE sql AS 'SELECT x';
SELECT dl830_echo('abcdef');
SELECT dl830_echo('abcdef'::text);
CREATE FUNCTION dl830_result() RETURNS dl830_short LANGUAGE sql AS $$SELECT 'abcdef'::text$$;
SELECT dl830_result();
CREATE FUNCTION dl830_wide() RETURNS dl830_num LANGUAGE sql AS 'SELECT 12345';
SELECT dl830_wide();
CREATE FUNCTION dl830_plpgsql() RETURNS dl830_short LANGUAGE plpgsql AS $$BEGIN RETURN 'abcdef'; END$$;
SELECT dl830_plpgsql();
SELECT array_append(ARRAY['a']::dl830_short[], 'abcdef');
SELECT array_append(ARRAY['a']::dl830_short[], 'abcdef'::text);
SELECT array_cat(ARRAY['a']::dl830_short[], '{abcdef}');
PREPARE dl830_scalar(dl830_short) AS SELECT $1;
EXECUTE dl830_scalar('abcdef');
PREPARE dl830_array(dl830_short[]) AS SELECT $1;
EXECUTE dl830_array('{abcdef}');
SELECT pg_input_is_valid('abcdef', 'dl830_short');
-- PL/pgSQL assignments reject; a declared cast truncates.
DO $$DECLARE v dl830_short; BEGIN v := 'abcdef'; END$$;
DO $$DECLARE v dl830_short; t text := 'abcdef'; BEGIN v := t; END$$;
DO $$DECLARE v dl830_num; BEGIN v := 12345; END$$;
DO $$DECLARE v dl830_short := 'abcdef'::dl830_short; BEGIN RAISE NOTICE '%', v; END$$;
-- A constant's length coercion fails while planned, even without rows; the
-- domain's own check waits for a row.
SELECT '12345'::dl830_num FROM dl830_empty;
SELECT '-1'::dl830_pos FROM dl830_empty;
UPDATE dl830_empty SET b = 'abcdef';
UPDATE dl830_empty SET c = 'abcdef';
UPDATE dl830_empty SET d = -1;
-- Array input applies the element domain's rules at the literal.
SELECT '{abcdef}'::dl830_short[];
SELECT 1, '{abcdef}'::dl830_short[];
SELECT 'abcdef'::dl830_short[];
SELECT '{-1}'::dl830_pos[];
SELECT '{x}'::dl830_pos[];
SELECT '{1,2'::dl830_pos[];
INSERT INTO dl830 (b) VALUES ('{abcdef}');
INSERT INTO dl830 (p) VALUES ('{-1}');
INSERT INTO dl830 (p) VALUES ('{x}');
PREPARE dl830_literal AS SELECT '{abcdef}'::dl830_short[];
