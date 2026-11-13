-- Whole-row index keys use the table's row type: record ordering, NULL
-- fields comparing equal for uniqueness, and the row's stored form.
CREATE TABLE wk353 (a int, b text);
INSERT INTO wk353 VALUES (1, 'x'), (2, NULL), (1, 'x');
CREATE INDEX wk353_whole ON wk353 ((wk353));
CREATE UNIQUE INDEX wk353_unique ON wk353 ((wk353));
DELETE FROM wk353 WHERE a = 1;
INSERT INTO wk353 VALUES (1, 'x');
CREATE UNIQUE INDEX wk353_unique ON wk353 ((wk353));
CREATE INDEX ON wk353 (wk353);
CREATE INDEX ON wk353 ((wk353.*));
INSERT INTO wk353 VALUES (1, 'x');
INSERT INTO wk353 VALUES (3, 'y'), (NULL, NULL);
INSERT INTO wk353 VALUES (NULL, NULL);
INSERT INTO wk353 VALUES (NULL, 'q'), (NULL, 'q');
SELECT a, b, wk353 IS NULL FROM wk353 ORDER BY wk353;
SELECT pg_get_indexdef(indexrelid), indkey, indexprs IS NOT NULL
  FROM pg_index WHERE indrelid = 'wk353'::regclass ORDER BY indexrelid;
\d wk353
SELECT * FROM wk353 WHERE wk353 = '(1,x)'::wk353;
SELECT * FROM wk353 w WHERE w = '(3,y)'::wk353;
SELECT * FROM wk353 WHERE wk353 = ROW(1, 'x')::wk353;
UPDATE wk353 SET b = 'x' WHERE a = 3;
UPDATE wk353 SET b = 'z' WHERE a = 3;
UPDATE wk353 SET a = 1, b = 'x' WHERE a = 2;
CREATE INDEX ON wk353 ((ROW(a, b)));
CREATE INDEX ON wk353 ((ROW(a, b)::wk353));
REINDEX TABLE wk353;
SELECT a, b FROM wk353 ORDER BY wk353 DESC;
DROP TABLE wk353;
