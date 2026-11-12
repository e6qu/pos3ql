-- Rule INSERT actions writing DEFAULT take each column's default, identity
-- and generated value; an untyped literal compared with a row subquery's
-- field takes that field's type.
CREATE TABLE src219 (a int, b text);
CREATE TABLE log219 (id serial, a int, note text DEFAULT 'dflt', g int GENERATED ALWAYS AS (a * 2) STORED, i int GENERATED ALWAYS AS IDENTITY);
CREATE RULE r1 AS ON INSERT TO src219 DO ALSO INSERT INTO log219 (a, note, g, i) VALUES (new.a, DEFAULT, DEFAULT, DEFAULT);
INSERT INTO src219 VALUES (1, 'x'), (2, 'y');
CREATE RULE r2 AS ON UPDATE TO src219 DO ALSO INSERT INTO log219 VALUES (DEFAULT, old.a, DEFAULT);
UPDATE src219 SET b = 'z' WHERE a = 1;
CREATE RULE r3 AS ON DELETE TO src219 DO ALSO INSERT INTO log219 (a, note) VALUES (old.a, 'one'), (old.a + 100, DEFAULT);
DELETE FROM src219 WHERE a = 2;
SELECT * FROM log219 ORDER BY id;
DROP TABLE src219, log219;
SELECT (1, 'a') = (SELECT 1, 2);
SELECT (1, '2') = (SELECT 1, 2), ('1', 3) < (SELECT 1, 4);
SELECT (SELECT 1, 2) = (1, '2');
