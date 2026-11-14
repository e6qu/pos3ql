-- A query whose condition an index answers reads rows through their index
-- entries: an updated row comes back at its first position, where a
-- sequential scan returns it after the rows it moved past.
CREATE TABLE io471 (id int PRIMARY KEY, v text);
INSERT INTO io471 VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd');
UPDATE io471 SET v = v || '!' WHERE id = 2;
UPDATE io471 SET v = v || '?' WHERE v = 'a';
TABLE io471;
SELECT * FROM io471 WHERE id > 0;
SELECT * FROM io471 WHERE id >= 1 AND id < 4;
SELECT * FROM io471 WHERE id IN (1, 2, 3);
SELECT * FROM io471 WHERE id = 2 OR id = 1;
SELECT * FROM io471 WHERE v > '';
SELECT v FROM io471 WHERE id > 1 AND v <> 'x';
CREATE TABLE ix471 (id int, v text);
CREATE INDEX ix471_id ON ix471 (id);
INSERT INTO ix471 VALUES (3, 'c'), (1, 'a'), (2, 'b');
UPDATE ix471 SET v = 'x' WHERE v = 'c';
TABLE ix471;
SELECT * FROM ix471 WHERE id > 0;
SELECT * FROM ix471 WHERE id = 3 OR id = 1;
SELECT * FROM ix471 WHERE v <> '';
-- Without a usable index, an INSERT reads its source in heap order.
CREATE TABLE src471 (a int);
INSERT INTO src471 VALUES (1), (2), (3);
UPDATE src471 SET a = a WHERE a < 3;
CREATE TABLE dst471 (a int);
INSERT INTO dst471 SELECT a FROM src471;
TABLE dst471;
INSERT INTO src471 SELECT a + 10 FROM src471;
TABLE src471;
