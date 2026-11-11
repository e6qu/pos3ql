-- Assignment through composite fields and array elements in UPDATE, INSERT,
-- ON CONFLICT and MERGE, with assignment coercion, merged targets of one
-- column and PostgreSQL's analysis errors.
CREATE TYPE fa111_pair AS (x int, y text);
CREATE TYPE fa111_nest AS (p fa111_pair, n int);
CREATE TYPE fa111_bag AS (x int, arr int[], t text);
CREATE DOMAIN fa111_positive AS int CHECK (VALUE > 0);
CREATE TYPE fa111_checked AS (d fa111_positive);
CREATE TABLE fa111 (id int PRIMARY KEY, p fa111_pair, a fa111_pair[], n fa111_nest,
  i int, b fa111_bag, c fa111_checked);
INSERT INTO fa111 (id, i) VALUES (1, 0);
UPDATE fa111 SET i = 1, i = 2;
UPDATE fa111 SET p.x = 1, p = NULL;
UPDATE fa111 SET p = NULL, p.x = 1;
UPDATE fa111 SET p.x = 1, p.x = 2 RETURNING p;
UPDATE fa111 SET p.x = '7', p.y = 5 RETURNING p;
UPDATE fa111 SET p.x = 'a' RETURNING p;
UPDATE fa111 SET p.x = 2.7 RETURNING p;
UPDATE fa111 SET p.z = 1;
UPDATE fa111 SET i.z = 1;
UPDATE fa111 SET p.x = now();
UPDATE fa111 SET n.p.x = 3, n.n = 4 RETURNING n;
UPDATE fa111 SET a[2].x = 5 RETURNING a;
UPDATE fa111 SET a[2].y = 'q' RETURNING a;
UPDATE fa111 SET a[1].x = now();
UPDATE fa111 SET n.p.z = 1;
UPDATE fa111 SET fa111.i = 1;
UPDATE fa111 AS f SET f.i = 1;
UPDATE fa111 SET (p.x, p.y) = (8, 'z') RETURNING p;
UPDATE fa111 SET p.x = p.x + 1 RETURNING p;
UPDATE fa111 SET p.x = DEFAULT;
UPDATE fa111 SET b.arr[1] = DEFAULT;
UPDATE fa111 SET b.arr[2] = 9 RETURNING b;
UPDATE fa111 SET b.arr[1] = 'x';
UPDATE fa111 SET b.x[1] = 3;
UPDATE fa111 SET b.arr[1].x = 1;
UPDATE fa111 SET b.arr[1:2] = '{1,2}' RETURNING b;
UPDATE fa111 SET b.arr[:1] = '{7}' RETURNING b;
UPDATE fa111 SET b.arr[1:2] = '{a}';
UPDATE fa111 SET b.x = 1, b.arr[1] = 5 RETURNING b;
UPDATE fa111 SET b.arr[1] = 8, b.x = 2 RETURNING b;
UPDATE fa111 SET p.x = NULL RETURNING p;
INSERT INTO fa111 (id, p.x, p.y) VALUES (2, 9, 'w') RETURNING p;
INSERT INTO fa111 (id, p.y) VALUES (3, 'only') RETURNING p;
INSERT INTO fa111 (id, n.p.x) VALUES (4, 1) RETURNING n;
INSERT INTO fa111 (id, p, p.x) VALUES (5, NULL, 1);
INSERT INTO fa111 (id, p.x, p) VALUES (5, 1, NULL);
INSERT INTO fa111 (id, i, i) VALUES (5, 1, 1);
INSERT INTO fa111 (id, p.x, p.y) SELECT 6, 1, 'sel' RETURNING p;
INSERT INTO fa111 (id, a[1].x) VALUES (7, 3) RETURNING a;
INSERT INTO fa111 (p.z) VALUES (1);
INSERT INTO fa111 (p.x) VALUES ('bad');
INSERT INTO fa111 (id, p.x) VALUES (8, DEFAULT);
INSERT INTO fa111 (id, c.d) VALUES (9, 5) RETURNING c;
INSERT INTO fa111 (id, c.d) VALUES (10, -5);
INSERT INTO fa111 (id, p.x) VALUES (1, 77)
  ON CONFLICT (id) DO UPDATE SET p.y = 'conflict', p.x = EXCLUDED.i RETURNING p;
MERGE INTO fa111 USING (SELECT 1 AS id) s ON fa111.id = s.id
  WHEN MATCHED THEN UPDATE SET p.y = 'merged' RETURNING p;
MERGE INTO fa111 USING (SELECT 11 AS id) s ON fa111.id = s.id
  WHEN NOT MATCHED THEN INSERT (id, p.x, p.y) VALUES (11, 4, 'm');
PREPARE fa111_set(int) AS UPDATE fa111 SET p.x = $1 WHERE id = 1 RETURNING p;
EXECUTE fa111_set(42);
SELECT id, p, a, n, b, c FROM fa111 ORDER BY id;
-- A three-part column reference reports its position.
SELECT q.p.x FROM fa111;
