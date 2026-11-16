-- An outer join null-extends the outer rows a partitioned table or
-- inheritance tree matches nowhere, including when pruning leaves it no
-- member, and a RIGHT or FULL join returns each member's unmatched rows.
CREATE TABLE pt611 (id int, v int) PARTITION BY RANGE (id);
CREATE TABLE pt611_1 PARTITION OF pt611 FOR VALUES FROM (1) TO (10);
CREATE TABLE pt611_2 PARTITION OF pt611 FOR VALUES FROM (10) TO (20);
INSERT INTO pt611 SELECT g, g FROM generate_series(1, 19) g;
CREATE TABLE o611 (k int, w int);
INSERT INTO o611 VALUES (5, 1), (15, 2);
SELECT * FROM o611 LEFT JOIN pt611 ON pt611.id = o611.k AND pt611.id > 100 ORDER BY k;
SELECT * FROM o611 LEFT JOIN pt611 ON pt611.id = o611.k AND pt611.id > 12 ORDER BY k;
SELECT * FROM pt611 RIGHT JOIN o611 ON pt611.id = o611.k AND pt611.id > 100 ORDER BY k;
SELECT * FROM o611 FULL JOIN pt611 ON pt611.id = o611.k AND pt611.id > 100 ORDER BY k, id;
SELECT * FROM o611 WHERE NOT EXISTS (SELECT 1 FROM pt611 WHERE pt611.id = o611.k AND pt611.id > 100);
SELECT * FROM o611 WHERE NOT EXISTS (SELECT 1 FROM pt611 WHERE pt611.id = o611.k AND pt611.id > 12);
SELECT * FROM o611 LEFT JOIN pt611 ON pt611.id = o611.k WHERE pt611.id > 100;
SELECT * FROM o611 LEFT JOIN pt611 ON pt611.id = o611.k AND o611.w > 100 ORDER BY k;
SELECT * FROM o611 LEFT JOIN pt611 ON pt611.id = o611.k AND pt611.v > 100 ORDER BY k;
SELECT * FROM o611 RIGHT JOIN pt611 ON pt611.id = o611.k ORDER BY id;
SELECT * FROM o611 FULL JOIN pt611 ON pt611.id = o611.k AND pt611.v < 10 ORDER BY id, k;
SELECT pt611.tableoid::regclass, * FROM o611 RIGHT JOIN pt611 ON pt611.id = o611.k WHERE pt611.id > 12 ORDER BY id;
SELECT * FROM o611 FULL JOIN (SELECT * FROM pt611 WHERE id < 3) p ON p.id = o611.k ORDER BY id, k;
SELECT * FROM o611 x FULL JOIN pt611 ON pt611.id = x.k FULL JOIN o611 y ON y.w = pt611.v ORDER BY 1, 3, 5;
CREATE TABLE ip611 (a int, v int);
CREATE TABLE ip611_1 (CHECK (a < 10)) INHERITS (ip611);
INSERT INTO ip611 VALUES (1, 1), (5, 5);
INSERT INTO ip611_1 VALUES (2, 2), (15, 3);
SELECT * FROM o611 FULL JOIN ip611 ON ip611.a = o611.k ORDER BY a, k;
SELECT * FROM o611 LEFT JOIN ip611 ON ip611.a = o611.k ORDER BY a, k;
SELECT * FROM o611 RIGHT JOIN ip611 ON ip611.a = o611.k AND ip611.v > 1 ORDER BY a, k;
