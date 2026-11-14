-- The planner folds the constants of join conditions, of a function a
-- LATERAL item calls for each row, of ON CONFLICT DO UPDATE and of MERGE
-- once, before any row is read: each literal conversion raises its notice
-- once however many rows reach it.
CREATE TABLE c513 (id int PRIMARY KEY, q tsquery, n int);
INSERT INTO c513 VALUES (1, 'a', 0), (2, 'b', 0), (3, 'c', 0);
CREATE TABLE c513_s (id int, v int);
INSERT INTO c513_s VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50);
SELECT c513.id FROM c513 JOIN c513_s s ON c513.id = s.id AND numnode(''::tsquery) = 0
 ORDER BY 1;
SELECT c513.id FROM c513 JOIN c513_s s ON c513.id = s.id + numnode(''::tsquery) ORDER BY 1;
SELECT c513.id, s.v FROM c513 LEFT JOIN c513_s s
    ON c513.id = s.id AND numnode(''::tsquery) + c513.id > 1
 ORDER BY 1;
SELECT c513.id, g FROM c513, LATERAL generate_series(1, numnode(''::tsquery) + c513.id) g
 ORDER BY 1, 2;
INSERT INTO c513 SELECT id, 'z', 0 FROM c513_s WHERE id < 3
    ON CONFLICT (id) DO UPDATE SET q = ''::tsquery, n = numnode(''::tsquery)
    WHERE numnode('x'::tsquery) = 1;
INSERT INTO c513 VALUES (1, 'z', 0), (2, 'y', 0) ON CONFLICT (id) DO UPDATE SET n = 1 / 1;
MERGE INTO c513 USING c513_s s ON c513.id = s.id
 WHEN MATCHED AND numnode(''::tsquery) = 0 THEN
   UPDATE SET q = ''::tsquery, n = numnode('x'::tsquery)
 WHEN NOT MATCHED THEN INSERT VALUES (s.id, ''::tsquery, 0);
MERGE INTO c513 USING c513_s s ON c513.id = s.id AND numnode(''::tsquery) = 0
 WHEN MATCHED AND s.id > 3 THEN DELETE;
SELECT id, q, n FROM c513 ORDER BY id;
-- A constant that fails folds and fails even when no row reaches it.
SELECT c513.id FROM c513 JOIN c513_s s ON c513.id = s.id AND 1 / 0 = 1 WHERE false;
MERGE INTO c513 USING c513_s s ON c513.id = s.id
 WHEN MATCHED AND s.id > 100 THEN UPDATE SET n = 1 / 0;
