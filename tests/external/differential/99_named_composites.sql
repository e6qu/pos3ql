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
