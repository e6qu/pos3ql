-- Durable named composite types: catalog identity, nested values, query
-- propagation, and transactional DDL all match PostgreSQL's SQL boundary.
CREATE TYPE composite_coordinate AS (x integer, y integer);
CREATE TYPE composite_place AS (name text, coordinate composite_coordinate);

CREATE TABLE composite_places (id integer, place composite_place, places composite_place[]);
INSERT INTO composite_places VALUES
  (1, ROW('park', ROW(3, 4)::composite_coordinate)::composite_place,
      ARRAY[ROW('park', ROW(3, 4)::composite_coordinate)::composite_place]),
  (2, ROW('square', ROW(7, 8)::composite_coordinate)::composite_place,
      ARRAY[ROW('square', ROW(7, 8)::composite_coordinate)::composite_place]);

SELECT typname, typtype, typcategory, typelem <> 0
  FROM pg_type
 WHERE typname IN ('composite_coordinate', '_composite_coordinate',
                   'composite_place', '_composite_place')
 ORDER BY typname;
SELECT attname, atttypid = (SELECT oid FROM pg_type WHERE typname = 'composite_coordinate')
  FROM pg_attribute
 WHERE attrelid = (SELECT typrelid FROM pg_type WHERE typname = 'composite_place')
 ORDER BY attnum;
SELECT (place).name, ((place).coordinate).x, pg_typeof(place), pg_typeof(places[1])
  FROM composite_places
 ORDER BY id;
WITH copied AS (SELECT place FROM composite_places)
SELECT (place).name FROM copied ORDER BY 1;
SELECT place, count(*) FROM composite_places GROUP BY place ORDER BY place;
SELECT (place).name, row_number() OVER (ORDER BY id) FROM composite_places ORDER BY id;
SELECT place FROM composite_places WHERE id = 1
UNION
SELECT place FROM composite_places WHERE id = 2
ORDER BY 1;
UPDATE composite_places
   SET place = ROW('garden', ROW(9, 10)::composite_coordinate)::composite_place
 WHERE id = 1
 RETURNING (place).name, ((place).coordinate).y;

BEGIN;
CREATE TYPE composite_rollback AS (value integer);
ROLLBACK;
SELECT count(*) FROM pg_type WHERE typname = 'composite_rollback';

-- Attribute identity is physical: historical nested and array values retain
-- their original fields while the public layout evolves.
CREATE TYPE composite_evolving AS (left_value integer, removed_value text);
CREATE TABLE composite_evolving_values (value composite_evolving, values composite_evolving[]);
INSERT INTO composite_evolving_values VALUES
  (ROW(7, 'old')::composite_evolving, ARRAY[ROW(8, 'array-old')::composite_evolving]);
CREATE INDEX composite_evolving_value_idx ON composite_evolving_values (value);
ALTER TYPE composite_evolving ADD ATTRIBUTE right_value integer;
ALTER TYPE composite_evolving RENAME ATTRIBUTE left_value TO retained_value;
ALTER TYPE composite_evolving DROP ATTRIBUTE removed_value;
SELECT (value).retained_value, (value).right_value IS NULL,
       (values[1]).retained_value, (values[1]).right_value IS NULL
  FROM composite_evolving_values;
SELECT attnum, attisdropped
  FROM pg_attribute
 WHERE attrelid = (SELECT typrelid FROM pg_type WHERE typname = 'composite_evolving')
 ORDER BY attnum;
INSERT INTO composite_evolving_values VALUES (ROW(8, 9)::composite_evolving, NULL);
SELECT (value).retained_value FROM composite_evolving_values
 WHERE value = ROW(8, 9)::composite_evolving;

CREATE TYPE composite_drop_root AS (value integer);
CREATE TYPE composite_drop_leaf AS (root composite_drop_root);
CREATE TABLE composite_drop_values (id integer, root composite_drop_root, roots composite_drop_root[]);
CREATE VIEW composite_drop_view AS SELECT ROW(1)::composite_drop_root AS root;
DROP TYPE composite_drop_root;
DROP TYPE composite_drop_root CASCADE;
SELECT typname FROM pg_type
 WHERE typname IN ('composite_drop_root', 'composite_drop_leaf') ORDER BY typname;
SELECT attname FROM pg_attribute
 WHERE attrelid = 'composite_drop_values'::regclass
   AND attnum > 0 AND NOT attisdropped ORDER BY attnum;
