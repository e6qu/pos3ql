-- Exclusion expression elements survive REINDEX and renames, and a
-- partitioned table's new constraint is built on each existing partition.
CREATE TABLE ri451 (a int, b int,
  CONSTRAINT ri451_x EXCLUDE USING gist (int4range(a, b) WITH &&,
                                         int4range(b, b, '[]') WITH -|-)
    WHERE (a > 0));
INSERT INTO ri451 VALUES (1, 3);
REINDEX INDEX ri451_x;
REINDEX TABLE ri451;
\d ri451
SELECT pg_get_indexdef('ri451_x'::regclass), indkey, indnatts
  FROM pg_index WHERE indexrelid = 'ri451_x'::regclass;
SELECT pg_get_constraintdef(oid, true), conkey FROM pg_constraint
 WHERE conname = 'ri451_x';
INSERT INTO ri451 VALUES (2, 4);
INSERT INTO ri451 VALUES (4, 6);
ALTER TABLE ri451 RENAME COLUMN b TO bb;
\d ri451
CREATE TABLE pp451 (a int, b int, p int4range) PARTITION BY LIST (p);
CREATE TABLE pp451_1 PARTITION OF pp451 FOR VALUES IN ('[1,2)');
INSERT INTO pp451 VALUES (1, 5, '[1,2)'), (3, 7, '[1,2)');
ALTER TABLE pp451 ADD CONSTRAINT pp451_x
  EXCLUDE USING gist (p WITH =, int4range(a, b) WITH &&);
ALTER TABLE pp451 ADD CONSTRAINT pp451_k UNIQUE (p, a);
DELETE FROM pp451 WHERE a = 3;
ALTER TABLE pp451 ADD CONSTRAINT pp451_x
  EXCLUDE USING gist (p WITH =, int4range(a, b) WITH &&);
\d pp451_1
ALTER TABLE pp451 DETACH PARTITION pp451_1;
\d pp451_1
INSERT INTO pp451_1 VALUES (2, 3, '[1,2)');
DROP TABLE ri451, pp451, pp451_1;
