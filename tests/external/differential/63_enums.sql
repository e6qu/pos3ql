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
DROP SCHEMA IF EXISTS type_owner_first CASCADE;
DROP SCHEMA IF EXISTS type_owner_later CASCADE;
DROP ROLE IF EXISTS composite_type_owner;

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

-- Rewrites retain both committed and transaction-pending rows. Savepoint
-- rollback restores scalar/array values and indexed lookups together.
CREATE TYPE rewrite_state AS ENUM ('old', 'ready');
CREATE TABLE rewrite_scalar (id integer PRIMARY KEY, state rewrite_state, states rewrite_state[]);
CREATE INDEX rewrite_state_index ON rewrite_scalar(state);
CREATE TABLE rewrite_array (id integer PRIMARY KEY, states rewrite_state[]);
INSERT INTO rewrite_scalar VALUES
  (1, 'old', ARRAY['old', 'ready']::rewrite_state[]),
  (2, 'ready', ARRAY['ready']::rewrite_state[]), (3, NULL, NULL);
INSERT INTO rewrite_array VALUES
  (10, ARRAY['old', NULL, 'ready']::rewrite_state[]), (11, NULL);
BEGIN;
UPDATE rewrite_scalar SET state = 'old', states = ARRAY['old']::rewrite_state[] WHERE id = 2;
INSERT INTO rewrite_scalar VALUES (4, 'old', ARRAY['ready', 'old']::rewrite_state[]);
DELETE FROM rewrite_scalar WHERE id = 3;
INSERT INTO rewrite_array VALUES (12, ARRAY['old']::rewrite_state[]);
SAVEPOINT before_rename;
ALTER TYPE rewrite_state RENAME VALUE 'old' TO 'new';
SELECT id, state, states FROM rewrite_scalar ORDER BY id;
SELECT id, states FROM rewrite_array ORDER BY id;
SELECT id FROM rewrite_scalar WHERE state = 'new' ORDER BY id;
ROLLBACK TO SAVEPOINT before_rename;
SELECT id, state, states FROM rewrite_scalar ORDER BY id;
SELECT id, states FROM rewrite_array ORDER BY id;
ALTER TYPE rewrite_state RENAME VALUE 'old' TO 'new';
COMMIT;
SELECT id, state, states FROM rewrite_scalar ORDER BY id;
SELECT id, states FROM rewrite_array ORDER BY id;
SELECT id FROM rewrite_scalar WHERE state = 'new' ORDER BY id;
DROP TABLE rewrite_scalar;
DROP TABLE rewrite_array;
DROP TYPE rewrite_state;

-- Enum labels inside composites and their arrays change with the type too;
-- ordinary text with the same spelling stays unchanged.
CREATE TYPE enum_nested_state AS ENUM ('old', 'ready');
CREATE TYPE enum_nested_detail AS (state enum_nested_state, note text);
CREATE TABLE enum_nested_orders (
  id integer, detail enum_nested_detail, batch enum_nested_detail[]
);
INSERT INTO enum_nested_orders VALUES
  (1, ROW('old', 'old')::enum_nested_detail,
   ARRAY[ROW('old', 'old')::enum_nested_detail]);
BEGIN;
ALTER TYPE enum_nested_state RENAME VALUE 'old' TO 'new';
SELECT (detail).state, (batch[1]).state, (detail).note FROM enum_nested_orders;
ROLLBACK;
SELECT (detail).state, (batch[1]).state, (detail).note FROM enum_nested_orders;
ALTER TYPE enum_nested_state RENAME VALUE 'old' TO 'new';
SELECT (detail).state, (batch[1]).state, (detail).note FROM enum_nested_orders;
DROP TABLE enum_nested_orders;
DROP TYPE enum_nested_detail;
DROP TYPE enum_nested_state;

-- Stored typed constants keep the same member when its label changes.
CREATE TYPE enum_identity_state AS ENUM ('old', 'ready');
CREATE TABLE enum_identity_defaults (state enum_identity_state DEFAULT 'old');
CREATE VIEW enum_identity_view AS SELECT 'old'::enum_identity_state AS state;
CREATE DOMAIN enum_identity_domain AS enum_identity_state CHECK (VALUE = 'old');
CREATE TABLE enum_identity_domains (state enum_identity_domain);
ALTER TYPE enum_identity_state RENAME VALUE 'old' TO 'new';
INSERT INTO enum_identity_defaults DEFAULT VALUES;
INSERT INTO enum_identity_domains VALUES ('new');
SELECT state::text FROM enum_identity_defaults;
SELECT state::text FROM enum_identity_view;
SELECT state::text FROM enum_identity_domains;
DROP TABLE enum_identity_defaults;
DROP TABLE enum_identity_domains;
DROP VIEW enum_identity_view;
DROP DOMAIN enum_identity_domain;
DROP TYPE enum_identity_state;

-- The type namespace is shared by enums, domains, and named composites.
CREATE TYPE enum_composite_name AS (id integer);
ALTER TYPE feeling RENAME TO enum_composite_name;
DROP TYPE enum_composite_name;

CREATE TYPE composite_alterable AS (value integer);
ALTER TYPE composite_alterable ALTER ATTRIBUTE value TYPE bigint;
CREATE ROLE composite_type_owner;
ALTER TYPE composite_alterable OWNER TO composite_type_owner;
COMMENT ON TYPE composite_alterable IS 'standalone composite';
SELECT atttypid::regtype
  FROM pg_attribute
 WHERE attrelid = 'composite_alterable'::regclass AND attname = 'value';
SELECT 'composite_alterable'::regclass::text;
SELECT obj_description('composite_alterable'::regtype, 'pg_type');
SELECT pg_get_userbyid(typowner) FROM pg_type WHERE typname = 'composite_alterable';
CREATE SCHEMA type_owner_first;
CREATE SCHEMA type_owner_later;
GRANT CREATE ON SCHEMA type_owner_first, type_owner_later TO composite_type_owner;
CREATE TYPE type_owner_first.owner_target AS ENUM ('ready');
CREATE TYPE type_owner_later.owner_target AS (value integer);
SET search_path TO type_owner_first, type_owner_later, public;
ALTER TYPE owner_target OWNER TO composite_type_owner;
SET search_path TO type_owner_later, type_owner_first, public;
ALTER TYPE owner_target OWNER TO composite_type_owner;
RESET search_path;
SELECT n.nspname, pg_get_userbyid(t.typowner)
  FROM pg_type t
  JOIN pg_namespace n ON n.oid = t.typnamespace
 WHERE t.typname = 'owner_target'
 ORDER BY n.nspname;
ALTER TYPE type_owner_first.owner_target OWNER TO postgres;
ALTER TYPE type_owner_later.owner_target OWNER TO postgres;
DROP TYPE type_owner_first.owner_target;
DROP TYPE type_owner_later.owner_target;
DROP SCHEMA type_owner_first;
DROP SCHEMA type_owner_later;
ALTER TYPE composite_alterable OWNER TO postgres;
DROP TYPE composite_alterable;
DROP ROLE composite_type_owner;

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


-- Captured constants, domain enforcement and plain text follow distinct meanings.
CREATE TYPE enum_guard_state AS ENUM ('old', 'ready', 'high');
CREATE DOMAIN enum_guard_value AS enum_guard_state CHECK (VALUE = ANY('{old,ready}'::enum_guard_state[]));
CREATE DOMAIN enum_guard_batch AS enum_guard_state[] CHECK (cardinality(VALUE) = 1 AND VALUE[1] = 'old');
CREATE TYPE enum_guard_detail AS (state enum_guard_value, note text);
CREATE TABLE enum_guard_rows (id integer, state enum_guard_value, batch enum_guard_batch, detail enum_guard_detail);
INSERT INTO enum_guard_rows VALUES (1, 'old', '{old}', ROW('old', 'old')::enum_guard_detail);
SELECT id FROM enum_guard_rows WHERE state = 'old';
SELECT id FROM enum_guard_rows WHERE state = 'old'::enum_guard_state;
SELECT id FROM enum_guard_rows WHERE state = 'old'::enum_guard_value;
SELECT id FROM enum_guard_rows WHERE 'old' = state;
SELECT NULL::enum_guard_value = 'old';
SELECT 'old'::enum_guard_value = 'old';
SELECT id FROM enum_guard_rows WHERE state < 'ready';
SELECT state IN ('old') FROM enum_guard_rows;
SELECT state = ANY(ARRAY['old'::enum_guard_state]) FROM enum_guard_rows;
SELECT state BETWEEN 'old' AND 'ready' FROM enum_guard_rows;
SELECT state IS DISTINCT FROM 'old' FROM enum_guard_rows;
SELECT CASE state WHEN 'old' THEN 1 END FROM enum_guard_rows;
CREATE VIEW enum_guard_view AS SELECT id, 'old'::enum_guard_state AS pinned, 'old' AS note FROM enum_guard_rows WHERE state::enum_guard_state = 'old'::enum_guard_state;
INSERT INTO enum_guard_rows(id, state) VALUES (2, 'high');
INSERT INTO enum_guard_rows(id, detail) VALUES (2, ROW('high', 'old')::enum_guard_detail);
ALTER TYPE enum_guard_state RENAME VALUE 'old' TO 'new';
SELECT state::text, batch[1]::text, (detail).state::text, (detail).note FROM enum_guard_rows;
SELECT id, pinned::text, note FROM enum_guard_view;
INSERT INTO enum_guard_rows(id, state) VALUES (2, 'high');
ALTER TYPE enum_guard_state ADD VALUE 'old';
INSERT INTO enum_guard_rows(id, batch) VALUES (2, '{old}');
CREATE SCHEMA enum_guard_moved;
ALTER TYPE enum_guard_state SET SCHEMA enum_guard_moved;
SELECT id, pinned::text, note FROM enum_guard_view;
INSERT INTO enum_guard_rows(id, state, batch) VALUES (2, 'new', '{new}');
SELECT id, state::text, batch[1]::text FROM enum_guard_rows ORDER BY id;
SELECT position('p3c:' IN pg_get_constraintdef(oid)) = 0 FROM pg_constraint WHERE contypid = 'enum_guard_value'::regtype ORDER BY conname;
DROP VIEW enum_guard_view;
DROP TABLE enum_guard_rows;
DROP TYPE enum_guard_detail;
DROP DOMAIN enum_guard_batch;
DROP DOMAIN enum_guard_value;
DROP TYPE enum_guard_moved.enum_guard_state;
DROP SCHEMA enum_guard_moved;


-- A domain containing a domain array remains a scalar declaration.
CREATE TYPE kept_state AS ENUM ('old', 'high');
CREATE DOMAIN kept_inner AS kept_state CHECK (VALUE::text <> 'high');
CREATE DOMAIN kept_outer AS kept_inner[] CHECK (cardinality(VALUE) = 1);
CREATE TABLE kept_rows(batch kept_outer, elements kept_inner[]);
CREATE FUNCTION kept_echo(kept_outer) RETURNS kept_outer LANGUAGE SQL AS 'SELECT $1';
INSERT INTO kept_rows VALUES ('{old}', '{old}');
ALTER DOMAIN kept_inner RENAME TO kept_inner_new;
ALTER TYPE kept_state RENAME VALUE 'old' TO 'new';
CREATE SCHEMA kept_moved;
ALTER TYPE kept_state SET SCHEMA kept_moved;
SELECT pg_typeof(batch) = 'kept_outer'::regtype,
       pg_typeof(elements) = 'kept_inner_new[]'::regtype,
       pg_typeof(kept_echo(batch)) = 'kept_outer'::regtype,
       batch[1]::text, elements[1]::text FROM kept_rows;
INSERT INTO kept_rows(batch) VALUES ('{new,new}');
INSERT INTO kept_rows(batch) VALUES ('{high}');
INSERT INTO kept_rows(elements) VALUES ('{high}');
DROP TABLE kept_rows;
DROP FUNCTION kept_echo(kept_outer);
DROP DOMAIN kept_outer;
DROP DOMAIN kept_inner_new;
DROP TYPE kept_moved.kept_state;
DROP SCHEMA kept_moved;


-- Domain defaults keep typed members for existing and newly created columns.
CREATE TYPE default_state AS ENUM ('old', 'ready');
CREATE DOMAIN default_value AS default_state DEFAULT 'old'::default_state;
CREATE DOMAIN default_batch AS default_state[] DEFAULT '{old}'::default_state[];
CREATE FUNCTION default_echo(default_state) RETURNS default_state LANGUAGE SQL VOLATILE AS 'SELECT $1';
CREATE TABLE default_before (state default_value, states default_batch,
    echo_state default_state DEFAULT default_echo('old'));
CREATE VIEW default_view AS SELECT * FROM default_before;
ALTER VIEW default_view ALTER COLUMN echo_state SET DEFAULT default_echo('old');
ALTER TYPE default_state RENAME VALUE 'old' TO 'new';
ALTER TYPE default_state ADD VALUE 'old';
CREATE SCHEMA defaults_moved;
ALTER TYPE default_state SET SCHEMA defaults_moved;
CREATE TABLE default_after (state default_value, states default_batch,
    echo_state defaults_moved.default_state DEFAULT default_echo('old'));
INSERT INTO default_before DEFAULT VALUES;
INSERT INTO default_after DEFAULT VALUES;
INSERT INTO default_view DEFAULT VALUES;
SELECT state::text, states[1]::text, echo_state::text FROM default_before;
SELECT state::text, states[1]::text, echo_state::text FROM default_after;
SELECT typname, typdefault FROM pg_type WHERE typname IN ('default_value','default_batch') ORDER BY typname;
DROP VIEW default_view;
DROP TABLE default_before;
DROP TABLE default_after;
DROP DOMAIN default_value;
DROP DOMAIN default_batch;
DROP FUNCTION default_echo(defaults_moved.default_state);
DROP TYPE defaults_moved.default_state;
DROP SCHEMA defaults_moved;
