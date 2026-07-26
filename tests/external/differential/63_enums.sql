-- Enum types (CREATE TYPE ... AS ENUM), matching PostgreSQL 18. Ordered label
-- sets: creation, ordering by definition order (not label text), comparison
-- operators, casts, ALTER TYPE ADD VALUE (append + BEFORE/AFTER), invalid-label
-- errors, DROP TYPE dependency handling, and pg_type / pg_enum reflection.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS enum_t;
DROP TYPE IF EXISTS mood;
DROP TYPE IF EXISTS rainbow;

CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
CREATE TABLE enum_t (id int, m mood);
INSERT INTO enum_t VALUES (1, 'happy'), (2, 'sad'), (3, 'ok'), (4, 'happy');

-- Ordering is by the enum's definition order, never by label text.
SELECT id, m FROM enum_t ORDER BY m, id;

-- pg_typeof reports the enum on a bare column.
SELECT DISTINCT pg_typeof(m) FROM enum_t;

-- Comparison operators use the sort order; aggregates too.
SELECT id FROM enum_t WHERE m > 'sad' ORDER BY id;
SELECT 'sad'::mood < 'happy'::mood AS lt, 'ok'::mood = 'ok'::mood AS eq;
SELECT min(m), max(m) FROM enum_t;

-- Casts between enum and text.
SELECT ('happy'::mood)::text || '!' AS shout;
SELECT ('sad'::text)::mood AS coerced;

-- An invalid label is 22P02, on write and on cast.
INSERT INTO enum_t VALUES (5, 'furious');
SELECT 'nope'::mood;

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
INSERT INTO enum_t VALUES (6, 'ecstatic'), (7, 'meh'), (8, 'glum');
SELECT id, m FROM enum_t WHERE id >= 6 ORDER BY m;
SELECT enumlabel FROM pg_enum
  WHERE enumtypid = (SELECT oid FROM pg_type WHERE typname = 'mood')
  ORDER BY enumsortorder;

-- IF NOT EXISTS is a no-op on a duplicate label; a bare duplicate errors (42710).
ALTER TYPE mood ADD VALUE IF NOT EXISTS 'happy';
ALTER TYPE mood ADD VALUE 'happy';

-- DROP TYPE RESTRICT fails while a column still depends on the enum (2BP01).
DROP TYPE mood;

-- A second, independent enum; DROP works once nothing uses it.
CREATE TYPE rainbow AS ENUM ('red', 'green', 'blue');
SELECT 'green'::rainbow AS pick, 'red'::rainbow < 'blue'::rainbow AS ordered;
DROP TYPE rainbow;
SELECT typname FROM pg_type WHERE typname IN ('mood', 'rainbow') ORDER BY typname;

-- Cleanup.
DROP TABLE enum_t;
DROP TYPE mood;
