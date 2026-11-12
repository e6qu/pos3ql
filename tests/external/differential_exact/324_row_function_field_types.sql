-- COALESCE, NULLIF, GREATEST and LEAST of rows return the arguments' row
-- type, so a selected field keeps its declared type: a domain or varchar
-- column stays a domain or varchar, as PostgreSQL types it.
CREATE SCHEMA row_function_fields;
SET search_path = row_function_fields, public;
CREATE DOMAIN label AS text CHECK (VALUE <> 'bad');
CREATE TABLE items(id int, code varchar(5), name label);
INSERT INTO items VALUES (1, 'q', 'y'), (2, NULL, NULL);
SELECT (coalesce(i, i)).name, pg_typeof((coalesce(i, i)).name),
       pg_typeof((nullif(i, i)).code), (coalesce(NULL, i)).id
  FROM items i ORDER BY id;
SELECT (coalesce(i)).* FROM items i ORDER BY 1;
SELECT (greatest(i, i)).code, (least(i, i)).name FROM items i ORDER BY 1;
CREATE FUNCTION audit() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
  RAISE NOTICE '% % %', tg_op, (coalesce(new, old)).name,
    pg_typeof((coalesce(new, old)).name);
  RAISE NOTICE '%', pg_typeof((coalesce(new, old)).code);
  RAISE NOTICE '%', (SELECT pg_typeof((coalesce(new, old)).name));
  r := coalesce(new, old);
  RAISE NOTICE '%', pg_typeof(r.name);
  RETURN coalesce(new, old);
END $$;
CREATE TRIGGER audit BEFORE INSERT OR UPDATE OR DELETE ON items
  FOR EACH ROW EXECUTE FUNCTION audit();
INSERT INTO items VALUES (3, 'w', 'z');
UPDATE items SET id = id WHERE id = 3;
DELETE FROM items WHERE id = 3;
CREATE TYPE pair AS (x int, l label);
CREATE TABLE pairs(p pair);
INSERT INTO pairs VALUES (ROW(1, 'a'));
SELECT (coalesce(p, p)).l, pg_typeof((coalesce(p, p)).l),
       pg_typeof((nullif(p, NULL)).x) FROM pairs;
SELECT (coalesce(p, p)).nope FROM pairs;
DROP SCHEMA row_function_fields CASCADE;
