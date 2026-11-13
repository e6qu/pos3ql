-- Renaming a composite field rewrites the index keys that select it; rolling
-- back to a savepoint restores an index created earlier in the transaction.
BEGIN;
CREATE TYPE sr357 AS (x integer, y integer);
CREATE TABLE sr357_rows (value sr357);
INSERT INTO sr357_rows VALUES (ROW(3, 4));
CREATE INDEX sr357_x ON sr357_rows (((value).x));
SAVEPOINT before_rename;
ALTER TYPE sr357 RENAME ATTRIBUTE x TO renamed;
SELECT (value).renamed FROM sr357_rows;
ROLLBACK TO before_rename;
SELECT (value).x FROM sr357_rows WHERE (value).x = 3;
COMMIT;
SELECT pg_get_indexdef('sr357_x'::regclass);
ALTER TYPE sr357 RENAME ATTRIBUTE x TO east;
SELECT pg_get_indexdef('sr357_x'::regclass);
DROP TABLE sr357_rows;
DROP TYPE sr357;
