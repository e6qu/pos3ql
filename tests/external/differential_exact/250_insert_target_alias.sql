-- INSERT INTO t AS alias: the alias names the target in ON CONFLICT DO
-- UPDATE SET and WHERE and in RETURNING, and hides the table's own name.
CREATE TABLE t250 (k int PRIMARY KEY, x int, y text);
INSERT INTO t250 AS a (k, x, y) VALUES (1, 10, 'a') RETURNING a.k, a.x, a.*;
INSERT INTO t250 AS a VALUES (1, 5, 'b') ON CONFLICT (k) DO UPDATE SET x = a.x + excluded.x WHERE a.x > 0 RETURNING a.*, t250.k;
INSERT INTO t250 AS a VALUES (1, 5, 'b') ON CONFLICT (k) DO UPDATE SET x = t250.x + 1;
INSERT INTO t250 AS a VALUES (1, 5, 'b') ON CONFLICT (k) DO UPDATE SET x = a.x + 1 WHERE t250.x > 0;
INSERT INTO t250 AS a VALUES (1, 5, 'b') ON CONFLICT (k) DO UPDATE SET x = a.x + 1 RETURNING t250.x;
INSERT INTO t250 AS excluded VALUES (1, 5, 'b') ON CONFLICT (k) DO UPDATE SET x = excluded.x + 1;
INSERT INTO t250 AS a VALUES (1, 5, 'b') ON CONFLICT (k) DO UPDATE SET x = a.x + 1 WHERE a.x < 100 RETURNING *;
INSERT INTO public.t250 AS a VALUES (2, 1, 'c') RETURNING a.k;
INSERT INTO t250 a VALUES (3, 1, 'c');
INSERT INTO t250 AS a (a.k) VALUES (3);
INSERT INTO t250 AS a VALUES (4, 1, 'q') ON CONFLICT (k) DO NOTHING RETURNING a.k;
WITH w AS (INSERT INTO t250 AS a VALUES (5, 2, 'w') ON CONFLICT (k) DO UPDATE SET y = a.y || '!' RETURNING a.k, a.y) SELECT * FROM w;
WITH w AS (INSERT INTO t250 AS a VALUES (5, 2, 'w') ON CONFLICT (k) DO UPDATE SET y = a.y || '!' RETURNING a.k, a.y) SELECT * FROM w;
DO $$ DECLARE r int; BEGIN INSERT INTO t250 AS a VALUES (6, 3, 'p') RETURNING a.x * 10 INTO r; RAISE NOTICE 'r=%', r; END $$;
INSERT INTO t250 AS a VALUES ($1, 1, 'e') ON CONFLICT (k) DO UPDATE SET x = a.x + $2 RETURNING a.k, a.x \bind 7 4 \g
INSERT INTO t250 AS a VALUES ($1, 1, 'e') ON CONFLICT (k) DO UPDATE SET x = a.x + $2 RETURNING a.k, a.x \bind 7 4 \g
UPDATE t250 AS u SET x = x + 1 WHERE k = 6 RETURNING t250.k;
DELETE FROM t250 AS d WHERE k = 6 RETURNING t250.k;
UPDATE t250 SET x = x + $1 WHERE x + $2 > 0 AND k = 7 RETURNING x * $3 \bind 1 0 2 \g
UPDATE t250 AS u SET x = u.x + $1 RETURNING u.k + $2 \bind 1 0 \g
DELETE FROM t250 WHERE x + $1 = 7 RETURNING k \bind 0 \g
INSERT INTO t250 VALUES ($1, 1, 'f') ON CONFLICT (k) DO UPDATE SET x = $2, y = excluded.y || $3 \bind 2 9 ! \g
SELECT * FROM t250 ORDER BY k;
DROP TABLE t250;
