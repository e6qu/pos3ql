-- CLUSTER chooses one ordinary index as a relation's durable ordering. The
-- selection changes are catalog-visible, transactional, and reject the same
-- invalid target shapes as PostgreSQL 18.
DROP TABLE IF EXISTS cluster_rows;
DROP TABLE IF EXISTS cluster_other;

CREATE TABLE cluster_rows (id integer, label text);
INSERT INTO cluster_rows VALUES (3, 'three'), (1, 'one'), (2, 'two');
CREATE INDEX cluster_rows_id ON cluster_rows (id);
CREATE INDEX cluster_rows_label ON cluster_rows (label);

SELECT indexrelid::regclass::text, indisclustered
FROM pg_index
WHERE indrelid = 'cluster_rows'::regclass
ORDER BY indexrelid::regclass::text;

CLUSTER cluster_rows USING cluster_rows_id;
SELECT indexrelid::regclass::text, indisclustered
FROM pg_index
WHERE indrelid = 'cluster_rows'::regclass
ORDER BY indexrelid::regclass::text;

CLUSTER cluster_rows USING cluster_rows_label;
CLUSTER cluster_rows;
SELECT indexrelid::regclass::text, indisclustered
FROM pg_index
WHERE indrelid = 'cluster_rows'::regclass
ORDER BY indexrelid::regclass::text;

CREATE TABLE cluster_other (id integer);
CREATE INDEX cluster_other_id ON cluster_other (id);
CLUSTER cluster_rows USING cluster_other_id;
CLUSTER cluster_other;
CLUSTER;

BEGIN;
CLUSTER cluster_rows;
ROLLBACK;

DROP TABLE cluster_rows;
DROP TABLE cluster_other;
