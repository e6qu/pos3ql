-- DDL, DML, defaults, RETURNING, ALTER.
CREATE TABLE d2 (id int NOT NULL, name text DEFAULT 'anon', score float8 DEFAULT 1.5, ok bool);
INSERT INTO d2 (id) VALUES (1);
INSERT INTO d2 VALUES (2, 'bolt', DEFAULT, true), (3, 'nut', 7.25, false) RETURNING id, name, score;
SELECT * FROM d2 ORDER BY id;
UPDATE d2 SET score = score * 2 WHERE ok RETURNING id, score;
DELETE FROM d2 WHERE id = 1 RETURNING name;
SELECT count(*) FROM d2;
ALTER TABLE d2 ADD COLUMN tag text DEFAULT 'zz';
SELECT id, tag FROM d2 ORDER BY id;
ALTER TABLE d2 RENAME COLUMN tag TO label;
ALTER TABLE d2 DROP COLUMN ok;
ALTER TABLE d2 RENAME TO d2renamed;
SELECT id, name, score, label FROM d2renamed ORDER BY id;
DROP TABLE d2renamed;
PREPARE dq (int) AS SELECT $1 + 1;
EXECUTE dq(41);
DEALLOCATE dq;

-- PRIMARY KEY / UNIQUE enforcement.
CREATE TABLE pk2 (id int PRIMARY KEY, email text UNIQUE, note text);
INSERT INTO pk2 VALUES (1, 'a@x', 'first');
INSERT INTO pk2 VALUES (1, 'b@x', 'dup pk');
INSERT INTO pk2 VALUES (2, 'a@x', 'dup email');
INSERT INTO pk2 VALUES (2, NULL, 'null unique ok');
INSERT INTO pk2 VALUES (3, NULL, 'nulls never collide');
UPDATE pk2 SET id = 2 WHERE id = 1;
INSERT INTO pk2 (id) VALUES (NULL);
SELECT id, email FROM pk2 ORDER BY id;
DROP TABLE pk2;
