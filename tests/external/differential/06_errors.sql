-- Errors must carry the same SQLSTATEs.
SELECT 1/0;
SELECT 1.0/0;
SELECT * FROM missing_table;
CREATE TABLE e6 (id int NOT NULL, v text);
SELECT no_col FROM e6;
INSERT INTO e6 VALUES (NULL, 'x');
INSERT INTO e6 (nope) VALUES (1);
CREATE TABLE e6 (id int);
SELECT 'zap'::int;
SELECT 1::nosuchtype;
SELECT 9223372036854775807 + 1;
SELECT nosuchfunc(1);
SELECT count(*), id FROM e6;
SELECT 'x' FROM e6 e JOIN e6 f ON true;
DROP TABLE e6;
