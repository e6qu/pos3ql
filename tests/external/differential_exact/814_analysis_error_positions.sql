-- Error cursors PostgreSQL's parse analysis reports for operators, keys,
-- arrays, casts, calls, options and constraints.
CREATE TABLE ep814 (id integer, label text, tags integer[]);
INSERT INTO ep814 VALUES (1, 'a', '{1,2}'), (2, 'b', '{3}');
CREATE TYPE ep814_pair AS (n integer, note varchar(4));
CREATE DOMAIN ep814_short AS varchar(4);
CREATE TABLE ep814_holder (id integer, pair ep814_pair);
CREATE FUNCTION ep814_take(ep814_pair) RETURNS integer LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION ep814_add(a bigint, b bigint DEFAULT 2) RETURNS bigint LANGUAGE sql AS 'SELECT $1 + $2';
-- Pattern, IN-list and quantified operators at their operator or keyword.
SELECT id FROM ep814 WHERE id NOT LIKE 'a';
SELECT id FROM ep814 WHERE label ~* 1;
SELECT id FROM ep814 WHERE id IN ('a'::text, 'b');
SELECT id FROM ep814 WHERE id = ANY (ARRAY['a'::text]);
SELECT id FROM ep814 WHERE label LIKE ANY (SELECT id FROM ep814);
SELECT id FROM ep814 WHERE id IS DISTINCT FROM 'x'::text;
SELECT 1 = ANY (2);
SELECT 'q' = ANY (tags) FROM ep814;
-- Keys without equality or ordering at the key as written.
SELECT DISTINCT '{}'::json FROM ep814;
SELECT id FROM ep814 GROUP BY '{}'::json;
SELECT DISTINCT ON (id) id, label FROM ep814 ORDER BY label;
SELECT DISTINCT label FROM ep814 ORDER BY id;
SELECT '{}'::json UNION SELECT '[]'::json;
SELECT count(DISTINCT '{}'::json) FROM ep814;
SELECT id FROM ep814 ORDER BY 'x';
SELECT id FROM ep814 GROUP BY 1.5;
-- Array constructors at the element or the ARRAY.
SELECT ARRAY[];
SELECT ARRAY[1, 'x'::text];
SELECT ARRAY[ARRAY[1], ARRAY[true]];
SELECT ARRAY['1'::cstring]::integer[];
SELECT (ARRAY[1]).x;
SELECT tags[1].x FROM ep814;
-- Literals whose input fails inside a type's input function.
SELECT '(1,toolong)'::ep814_pair;
SELECT 'x'::unknown::integer;
SELECT INTERVAL '1-12' YEAR TO MONTH;
INSERT INTO ep814_holder VALUES (1, '(1,toolong)');
INSERT INTO ep814_holder VALUES (1, ROW(1));
INSERT INTO ep814_holder VALUES (1, ROW(1, 'a', 2));
SELECT ep814_take(ROW('x', 'a'));
SELECT ep814_take(NULL::record);
SELECT ep814_add(1, 'x');
SELECT ep814_add(b => 'y', a => 1);
SELECT label COLLATE "C" < label COLLATE "POSIX" FROM ep814;
SELECT ('{}'::jsonb)[1.5];
SELECT JSON(1);
SELECT xmlserialize(document 1 AS text);
SELECT xmlconcat(1);
-- Calls used against their kind, and window names.
SELECT count(*) FROM ep814 GROUP BY 1;
SELECT rank() FROM ep814;
SELECT abs(id) OVER () FROM ep814;
SELECT abs(id) FILTER (WHERE true) FROM ep814;
SELECT sum(id) OVER missing FROM ep814;
SELECT sum(id) OVER (missing ORDER BY id) FROM ep814;
SELECT sum(id) OVER w FROM ep814 WINDOW w AS (), w AS (ORDER BY id);
-- Relations, parameters and EXECUTE arguments.
SELECT ep814.* FROM ep814 e;
SELECT missing.* FROM ep814;
SELECT *;
SELECT ($1).a;
PREPARE ep814_p(integer) AS SELECT $1;
EXECUTE ep814_p('x');
EXECUTE ep814_p((SELECT 1));
EXECUTE ep814_p('1'::text);
EXECUTE ep814_p(1 / 0);
PREPARE ep814_u AS SELECT ($1).a;
-- Utility options and constraints at the repeated or rejected one.
CREATE ROLE ep814_role LOGIN NOLOGIN;
CREATE ROLE ep814_role UNENCRYPTED PASSWORD 'x';
CREATE FUNCTION ep814_f() RETURNS integer LANGUAGE sql IMMUTABLE STABLE AS 'SELECT 1';
DO LANGUAGE plpgsql 'BEGIN END' LANGUAGE plpgsql;
CREATE PUBLICATION ep814_pub WITH (publish = 'insert', publish = 'update');
CREATE DOMAIN ep814_d AS integer NOT NULL NOT NULL;
CREATE DOMAIN ep814_d AS integer CONSTRAINT c CHECK (VALUE > 0) NO INHERIT;
CREATE DOMAIN ep814_d AS integer DEFAULT 1 DEFAULT 2;
CREATE FUNCTION ep814_g(p ep814_pair) RETURNS text LANGUAGE sql AS 'SELECT p.missing';
BEGIN;
DECLARE ep814_c CURSOR FOR SELECT id + true FROM ep814;
ROLLBACK;
BEGIN;
DECLARE ep814_c CURSOR FOR SELECT id FROM ep814 WHERE 'maybe';
ROLLBACK;
