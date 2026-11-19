-- The ORDER BY over a set operation's result is a tuplesort of the combined
-- rows in the order the branches return them: ties on the key follow its
-- quicksort, or its merged runs once the rows outgrow work_mem.
CREATE TABLE l917 (a int, b int);
CREATE TABLE r917 (a int, b int);
INSERT INTO l917 SELECT g % 9, g FROM generate_series(1, 2500) g;
INSERT INTO r917 SELECT g % 9, -g FROM generate_series(1, 2500) g;
SET work_mem = '64kB';
SELECT a, b FROM l917 UNION ALL SELECT a, b FROM r917 ORDER BY a OFFSET 2500 LIMIT 30;
SELECT a, b FROM l917 UNION ALL SELECT a, b FROM r917 ORDER BY a OFFSET 100 LIMIT 30;
SET work_mem = '128kB';
SELECT a, b FROM l917 UNION ALL SELECT a, b FROM r917 ORDER BY a OFFSET 2500 LIMIT 30;
SET work_mem = '4MB';
SELECT a, b FROM l917 UNION ALL SELECT a, b FROM r917 ORDER BY a OFFSET 2500 LIMIT 30;
