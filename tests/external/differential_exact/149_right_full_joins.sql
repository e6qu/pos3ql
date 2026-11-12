-- RIGHT and FULL joins: matched rows, then the inner rows nothing matched,
-- NULL-extended. A FULL JOIN needs an equality between its two sides once
-- constant conditions fold away, as PostgreSQL plans it only as a merge or
-- hash join. A scrollable cursor over a join moves back and jumps by
-- reading its join again from the first row.
CREATE TABLE a (x int, s text);
CREATE TABLE b (y int, r text, x int);
INSERT INTO a VALUES (1, 'a1'), (2, 'a2'), (3, 'a3'), (NULL, 'an');
INSERT INTO b VALUES (2, 'b2', 1), (4, 'b4', 2), (3, 'b3', 9), (5, 'b5', NULL), (NULL, 'bn', 3), (2, 'b2b', 1);
SELECT * FROM a RIGHT JOIN b ON a.x = b.y ORDER BY r, s;
SELECT * FROM a RIGHT JOIN b ON a.x < b.y ORDER BY r, s;
SELECT * FROM a FULL JOIN b ON a.x = b.y ORDER BY r, s;
SELECT * FROM a FULL JOIN b ON a.x = b.y WHERE b.y IS NULL OR a.x IS NULL ORDER BY r, s;
SELECT * FROM a FULL JOIN b ON a.x = b.y AND a.s > 'a2' ORDER BY r, s;
SELECT * FROM a FULL JOIN b ON a.x = b.y AND a.x < b.y ORDER BY r, s;
SELECT * FROM a FULL JOIN b ON 1 = 1 AND a.x = b.y ORDER BY r, s;
SELECT * FROM a FULL JOIN b ON a.x + 1 = b.y ORDER BY r, s;
SELECT * FROM a FULL JOIN b ON b.y = a.x ORDER BY r, s;
SELECT count(*) FROM a FULL JOIN b ON true;
SELECT * FROM a FULL JOIN b ON false ORDER BY r, s;
SELECT * FROM a FULL JOIN b USING (x) ORDER BY r, s;
SELECT * FROM a FULL JOIN b ON a.x = b.y OR a.x = 1;
SELECT * FROM a FULL JOIN b ON a.x = 1;
SELECT * FROM a FULL JOIN b ON a.x = a.x;
SELECT * FROM a FULL JOIN b ON a.x = b.y + a.x;
SELECT * FROM a FULL JOIN b ON a.x IS NOT DISTINCT FROM b.y;
SELECT * FROM a FULL JOIN b ON (a.x = b.y) IS TRUE;
SELECT * FROM a FULL JOIN b ON random() < 2;
SELECT a.s, v.z FROM a RIGHT JOIN (VALUES (1), (3), (7)) v(z) ON v.z = a.x ORDER BY v.z;
SELECT a.s, v.z FROM a FULL JOIN (VALUES (1), (3), (7)) v(z) ON v.z = a.x ORDER BY v.z, a.s;
SELECT a.s, v.z FROM a RIGHT JOIN (VALUES (1), (3), (7)) v(z) ON v.z = a.x
UNION ALL SELECT 'end', 0;
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT a.s, b.r FROM a LEFT JOIN b ON a.x < b.y;
FETCH ALL FROM c;
FETCH BACKWARD 3 FROM c;
FETCH ABSOLUTE 2 FROM c;
FETCH FORWARD 3 FROM c;
FETCH RELATIVE -2 FROM c;
FETCH LAST FROM c;
FETCH FIRST FROM c;
COMMIT;
