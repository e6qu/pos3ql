-- NullTest reduction on NOT NULL columns: PostgreSQL's planner folds these
-- only where it can prove them at the top of a qual — a bare conjunct, or
-- an IS NOT NULL arm on the spine of a top-level OR (constant-true arm ⇒
-- true disjunction). Anything deeper is left to execution's left-to-right
-- short circuit, and the difference is observable through error timing:
-- an erroring expression behind a fold fires or does not. Every case here
-- was pinned against PostgreSQL 18 with EXPLAIN (VERBOSE).
DROP TABLE IF EXISTS nf;
CREATE TABLE nf (id int NOT NULL, a int, ts timestamptz);
INSERT INTO nf VALUES (1, 10, '2020-01-01'), (2, 0, NULL), (3, NULL, '2021-01-01');

-- Bare conjuncts fold.
SELECT count(*) FROM nf WHERE id IS NULL;
SELECT count(*) FROM nf WHERE id IS NOT NULL;
SELECT count(*) FROM nf WHERE a/0 = 1 AND id IS NULL;
SELECT count(*) FROM nf WHERE id IS NULL AND a/0 = 1;

-- A top-level OR with an IS NOT NULL arm is constant-true: the erroring
-- sibling arm is never evaluated, at any spine depth.
SELECT count(*) FROM nf WHERE a/0 = 1 OR id IS NOT NULL;
SELECT count(*) FROM nf WHERE id IS NOT NULL OR a/0 = 1;
SELECT count(*) FROM nf WHERE ts IS NULL OR (a/0 = 1 OR id IS NOT NULL);

-- An IS NULL arm does NOT collapse its OR: the erroring arm still runs.
SELECT count(*) FROM nf WHERE a/0 = 1 OR id IS NULL;

-- Inside a nested AND nothing folds; evaluation order decides which side
-- effect fires.
SELECT count(*) FROM nf WHERE ts IS NULL OR (a/0 = 1 AND id IS NULL);
SELECT count(*) FROM nf WHERE ts IS NULL OR (id IS NULL AND a/0 = 1);
SELECT count(*) FROM nf WHERE (id IS NULL AND a/0 = 1) OR ts IS NULL;
SELECT count(*) FROM nf WHERE (a/0 = 1 AND id IS NULL) OR ts IS NULL;

-- Under NOT nothing folds either.
SELECT count(*) FROM nf WHERE NOT (id IS NULL) AND a/0 = 1;

-- Nullable columns never fold, wherever they sit.
SELECT count(*) FROM nf WHERE a IS NULL;
SELECT count(*) FROM nf WHERE 1/0 = 1 OR a IS NOT NULL;

DROP TABLE nf;
