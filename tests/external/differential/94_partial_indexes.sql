-- Partial B-tree indexes keep only predicate-matching rows in their unique
-- key space. Definitions remain visible through PostgreSQL's catalogs.
DROP TABLE IF EXISTS partial_index_rows;
CREATE TABLE partial_index_rows (id integer, value text, active boolean);
CREATE UNIQUE INDEX partial_index_active_value
  ON partial_index_rows (value) WHERE active;

INSERT INTO partial_index_rows VALUES (1, 'same', true);
INSERT INTO partial_index_rows VALUES (2, 'same', false);
INSERT INTO partial_index_rows VALUES (3, 'same', false);
INSERT INTO partial_index_rows VALUES (4, 'same', true);

SELECT id, value, active FROM partial_index_rows ORDER BY id;
SELECT indexdef FROM pg_indexes WHERE indexname = 'partial_index_active_value';

-- The predicate is type-checked and column-bound at CREATE time, including
-- when the table is empty.
CREATE INDEX partial_index_bad_type ON partial_index_rows (value) WHERE id;
CREATE INDEX partial_index_bad_column ON partial_index_rows (value) WHERE missing = 1;

DROP TABLE partial_index_rows;
