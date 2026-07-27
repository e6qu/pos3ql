-- ON CONFLICT (upsert), matching PostgreSQL 18. Exercises the arbiter as a
-- first-class analysis step: a conflict is caught only on the inferred/named
-- unique constraint, a violation of any OTHER unique falls through to 23505,
-- RETURNING projects the post-update row, and the arbiter-resolution errors
-- fire independent of the data.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS oc_t;
DROP TABLE IF EXISTS oc_c;

CREATE TABLE oc_t (a int UNIQUE, b int UNIQUE, note text);
INSERT INTO oc_t VALUES (1, 10, 'x'), (2, 20, 'y');

-- DO UPDATE ... RETURNING returns the updated row (post-update values).
INSERT INTO oc_t VALUES (1, 10, 'z') ON CONFLICT (a) DO UPDATE SET note = 'upd'
  RETURNING a, b, note;

-- ON CONSTRAINT names the arbiter index directly.
INSERT INTO oc_t VALUES (99, 20, 'z') ON CONFLICT ON CONSTRAINT oc_t_b_key
  DO UPDATE SET note = 'byname' RETURNING a, b, note;

-- EXCLUDED is the proposed row; it is usable in SET and RETURNING. The main
-- query's WHERE can reference the existing row (oc_t).
INSERT INTO oc_t VALUES (2, 20, 'w') ON CONFLICT (b) DO UPDATE
  SET note = EXCLUDED.note WHERE oc_t.a < 100 RETURNING a, b, note;

-- A conflict on a DIFFERENT unique than the arbiter is NOT caught: it falls
-- through to a normal duplicate-key error (23505), exactly as PostgreSQL does.
INSERT INTO oc_t VALUES (1, 999, 'diff') ON CONFLICT (b) DO UPDATE SET note = 'no';

-- DO NOTHING with no target catches any unique violation (here on a).
INSERT INTO oc_t VALUES (1, 777, 'skip') ON CONFLICT DO NOTHING RETURNING a;
-- DO NOTHING that inserts a brand-new row returns it.
INSERT INTO oc_t VALUES (5, 50, 'new') ON CONFLICT DO NOTHING RETURNING a, b;

-- Arbiter-resolution errors, raised at analysis time regardless of the data:
-- DO UPDATE requires an arbiter …
INSERT INTO oc_t VALUES (1, 10, 'q') ON CONFLICT DO UPDATE SET note = 'q';
-- … the target columns must match a unique/exclusion constraint …
INSERT INTO oc_t VALUES (1, 10, 'q') ON CONFLICT (note) DO NOTHING;
-- … a target column must exist …
INSERT INTO oc_t VALUES (1, 10, 'q') ON CONFLICT (nope) DO NOTHING;
-- … and a named constraint must exist.
INSERT INTO oc_t VALUES (1, 10, 'q') ON CONFLICT ON CONSTRAINT nope DO NOTHING;

-- Composite-key arbiter: the target column set matches order-independently,
-- and DO UPDATE can also be named by the composite constraint.
CREATE TABLE oc_c (x int, y int, v text, PRIMARY KEY (x, y));
INSERT INTO oc_c VALUES (1, 2, 'a');
INSERT INTO oc_c VALUES (1, 2, 'b') ON CONFLICT (y, x) DO UPDATE SET v = EXCLUDED.v
  RETURNING x, y, v;
INSERT INTO oc_c VALUES (1, 2, 'c') ON CONFLICT ON CONSTRAINT oc_c_pkey
  DO UPDATE SET v = 'byname' RETURNING v;

-- A multi-row statement mixes an update and an insert; both appear in RETURNING.
INSERT INTO oc_c VALUES (1, 2, 'multi'), (3, 4, 'fresh')
  ON CONFLICT (x, y) DO UPDATE SET v = EXCLUDED.v RETURNING x, y, v;

-- Final state.
SELECT a, b, note FROM oc_t ORDER BY a;
SELECT x, y, v FROM oc_c ORDER BY x, y;

DROP TABLE oc_t;
DROP TABLE oc_c;
