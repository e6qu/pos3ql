-- A field selected from a row value has its declared type and collation
-- however the row is computed: a cast to a named composite or a domain over
-- one, a CASE or COALESCE over such rows, or a function returning one, also
-- while a join or an ORDER BY evaluates it for each row.
CREATE TYPE fc993 AS (label text COLLATE "POSIX", note text);
CREATE DOMAIN fd993 AS fc993;
CREATE FUNCTION ff993(text) RETURNS fc993 LANGUAGE sql AS $$ SELECT ROW($1, 'n')::fc993 $$;
CREATE TABLE ft993 (id int, label text, r fc993);
INSERT INTO ft993 VALUES (1, 'b', ROW('b', 'x')), (2, 'a', ROW('a', 'y')), (3, 'B', ROW('B', 'z'));
SELECT pg_collation_for((ROW('x', 'y')::fc993).label), pg_collation_for((ROW('x', 'y')::fc993).note);
SELECT pg_collation_for((ROW('x', 'y')::fd993).label);
SELECT pg_collation_for((CASE WHEN true THEN ROW('x', 'y')::fc993 END).label);
SELECT pg_collation_for((COALESCE(NULL, ROW('x', 'y')::fc993)).label);
SELECT pg_collation_for((ff993('x')).label);
SELECT (ROW('x', 'y')::fd993).*, (CASE WHEN true THEN ROW('x', 'y')::fc993 END).*;
SELECT pg_typeof((ROW('x', 'y')::fd993).note), (CASE WHEN false THEN ROW('x', 'y')::fd993 END).label IS NULL;
SELECT (ROW('x', 'y')::fd993).zz;
SELECT (CASE WHEN true THEN ROW('x', 'y')::fc993 END).zz;
SELECT id, pg_collation_for((ROW(label, 'n')::fc993).label), (ROW(label, 'n')::fc993).label FROM ft993 ORDER BY (ROW(label, 'n')::fc993).label;
SELECT a.id, b.id, pg_collation_for((COALESCE(a.r, b.r)).label) FROM ft993 a JOIN ft993 b ON a.id = b.id ORDER BY 1;
SELECT a.id, lower((ff993(b.label)).label) FROM ft993 a JOIN ft993 b ON a.id < b.id ORDER BY 1, 2;
SELECT id FROM ft993 WHERE (ROW(label, 'n')::fd993).label < 'b' ORDER BY id;
BEGIN;
DECLARE c993 CURSOR FOR SELECT id, pg_collation_for((ROW(label, 'n')::fd993).label) FROM ft993 ORDER BY id;
FETCH 2 FROM c993;
FETCH ALL FROM c993;
COMMIT;
DROP TABLE ft993;
DROP FUNCTION ff993(text);
DROP DOMAIN fd993;
DROP TYPE fc993;
