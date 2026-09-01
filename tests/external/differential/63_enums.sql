-- Enum types (CREATE TYPE ... AS ENUM), matching PostgreSQL 18. Ordered label
-- sets: creation, ordering by definition order (not label text), comparison
-- operators, casts, ALTER TYPE ADD VALUE (append + BEFORE/AFTER), invalid-label
-- errors, DROP TYPE dependency handling, and pg_type / pg_enum reflection.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS enum_t;
DROP TABLE IF EXISTS composite_type_values;
DROP TYPE IF EXISTS mood;
DROP TYPE IF EXISTS rainbow;
DROP TYPE IF EXISTS enum_composite_name;
DROP TYPE IF EXISTS composite_lifecycle;
DROP TYPE IF EXISTS composite_alterable;
DROP TYPE IF EXISTS composite_lifecycle_schema.composite_moved;
DROP SCHEMA IF EXISTS composite_lifecycle_schema CASCADE;

CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
CREATE TABLE enum_t (id int, m mood, moods mood[]);
INSERT INTO enum_t VALUES
  (1, 'happy', ARRAY['happy']::mood[]),
  (2, 'sad', ARRAY['sad','happy']::mood[]),
  (3, 'ok', ARRAY['ok']::mood[]),
  (4, 'happy', ARRAY['happy','ok']::mood[]);

-- Ordering is by the enum's definition order, never by label text.
SELECT id, m FROM enum_t ORDER BY m, id;

-- pg_typeof reports the enum on a bare column.
SELECT DISTINCT pg_typeof(m) FROM enum_t;
SELECT DISTINCT pg_typeof(moods) FROM enum_t;

-- Comparison operators use the sort order; aggregates too.
SELECT id FROM enum_t WHERE m > 'sad' ORDER BY id;
SELECT 'sad'::mood < 'happy'::mood AS lt, 'ok'::mood = 'ok'::mood AS eq;
SELECT min(m), max(m) FROM enum_t;

-- Casts between enum and text.
SELECT ('happy'::mood)::text || '!' AS shout;
SELECT ('sad'::text)::mood AS coerced;

-- An invalid label is 22P02, on write and on cast.
INSERT INTO enum_t VALUES (5, 'furious', '{}');
SELECT 'nope'::mood;
SELECT ARRAY['nope']::mood[];

-- pg_type reflects the enum: typtype 'e', typcategory 'E', 4-byte oid width.
SELECT typname, typtype, typcategory, typlen FROM pg_type WHERE typname = 'mood';

-- pg_enum lists the members in sort order.
SELECT enumlabel FROM pg_enum
  WHERE enumtypid = (SELECT oid FROM pg_type WHERE typname = 'mood')
  ORDER BY enumsortorder;

-- ALTER TYPE ADD VALUE: append, then insert before/after an existing label.
ALTER TYPE mood ADD VALUE 'ecstatic';
ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok';
ALTER TYPE mood ADD VALUE 'glum' AFTER 'sad';
INSERT INTO enum_t VALUES
  (6, 'ecstatic', ARRAY['ecstatic']::mood[]),
  (7, 'meh', ARRAY['meh']::mood[]),
  (8, 'glum', ARRAY['glum']::mood[]);
SELECT id, m FROM enum_t WHERE id >= 6 ORDER BY m;
SELECT enumlabel FROM pg_enum
  WHERE enumtypid = (SELECT oid FROM pg_type WHERE typname = 'mood')
  ORDER BY enumsortorder;

-- IF NOT EXISTS is a no-op on a duplicate label; a bare duplicate errors (42710).
ALTER TYPE mood ADD VALUE IF NOT EXISTS 'happy';
ALTER TYPE mood ADD VALUE 'happy';

-- Both rename forms preserve the type/member OIDs and rewrite stored scalar
-- and array output transactionally.
ALTER TYPE mood RENAME VALUE 'sad' TO 'blue';
SELECT id, m, moods FROM enum_t WHERE id = 2;
ALTER TYPE mood RENAME TO feeling;
SELECT id, m, moods, pg_typeof(m), pg_typeof(moods) FROM enum_t WHERE id = 2;
SELECT t.typname, t.typtype, e.typname AS element_type, a.typname AS array_type
  FROM pg_type t
  LEFT JOIN pg_type e ON e.oid = t.typelem
  LEFT JOIN pg_type a ON a.oid = t.typarray
 WHERE t.typname IN ('feeling', '_feeling') ORDER BY t.typname;

BEGIN;
ALTER TYPE feeling RENAME VALUE 'blue' TO 'azure';
ALTER TYPE feeling RENAME TO emotion;
ROLLBACK;
SELECT id, m, moods, pg_typeof(m) FROM enum_t WHERE id = 2;

-- A second, independent enum; DROP works once nothing uses it.
CREATE TYPE rainbow AS ENUM ('red', 'green', 'blue');
SELECT 'green'::rainbow AS pick, 'red'::rainbow < 'blue'::rainbow AS ordered;
DROP TYPE rainbow;
SELECT typname FROM pg_type WHERE typname IN ('mood', 'rainbow') ORDER BY typname;

-- The type namespace is shared by enums, domains, and named composites.
CREATE TYPE enum_composite_name AS (id integer);
ALTER TYPE feeling RENAME TO enum_composite_name;
DROP TYPE enum_composite_name;

CREATE TYPE composite_alterable AS (value integer);
ALTER TYPE composite_alterable ALTER ATTRIBUTE value TYPE bigint;
COMMENT ON TYPE composite_alterable IS 'standalone composite';
SELECT atttypid::regtype
  FROM pg_attribute
 WHERE attrelid = 'composite_alterable'::regclass AND attname = 'value';
SELECT 'composite_alterable'::regclass::text;
SELECT obj_description('composite_alterable'::regtype, 'pg_type');
DROP TYPE composite_alterable;

-- A named composite evolves through typed field identity. Existing scalar and
-- array values, nullability, type changes, schema moves, and renames retain
-- the current layout without reinterpreting historical fields by name.
CREATE TYPE composite_lifecycle AS (id integer, note text, retired integer);
CREATE TABLE composite_type_values (
  value composite_lifecycle,
  values composite_lifecycle[]
);
INSERT INTO composite_type_values VALUES
  (ROW(1, 'first', 7)::composite_lifecycle,
   ARRAY[ROW(2, 'array', 8)::composite_lifecycle]);
ALTER TYPE composite_lifecycle ADD ATTRIBUTE active boolean;
ALTER TYPE composite_lifecycle RENAME ATTRIBUTE note TO label;
ALTER TYPE composite_lifecycle DROP ATTRIBUTE retired;
ALTER TYPE composite_lifecycle ALTER ATTRIBUTE id TYPE bigint;
ALTER TYPE composite_lifecycle ALTER ATTRIBUTE id TYPE bigint CASCADE;
SELECT (value).id, (value).label, (value).active,
       pg_typeof(value), pg_typeof(values)
  FROM composite_type_values;
SELECT (values[1]).id, (values[1]).label, (values[1]).active
  FROM composite_type_values;
ALTER TYPE composite_lifecycle ALTER ATTRIBUTE active SET NOT NULL;
ALTER TYPE composite_lifecycle ALTER ATTRIBUTE active DROP NOT NULL;
CREATE SCHEMA composite_lifecycle_schema;
ALTER TYPE composite_lifecycle SET SCHEMA composite_lifecycle_schema;
ALTER TYPE composite_lifecycle_schema.composite_lifecycle RENAME TO composite_moved;
SELECT (value).id, (value).label, (value).active,
       (values[1]).id, pg_typeof(value), pg_typeof(values)
  FROM composite_type_values;

-- Cleanup.
DROP TABLE enum_t;
DROP TYPE feeling;
DROP TABLE composite_type_values;
DROP TYPE composite_lifecycle_schema.composite_moved;
DROP SCHEMA composite_lifecycle_schema;
