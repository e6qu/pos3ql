-- `GROUP BY <n>` names the n-th select-list column, as `ORDER BY <n>` does,
-- and the ungrouped-column error names the column it is about.

CREATE TABLE grouppos (a int, b text, c int);
INSERT INTO grouppos VALUES (1,'x',10),(1,'y',20),(2,'x',30),(2,NULL,40),(NULL,'z',50);

-- a position stands for the select-list expression, alias or not
SELECT a, count(*) FROM grouppos GROUP BY 1 ORDER BY 1;
SELECT a AS z, count(*) FROM grouppos GROUP BY 1 ORDER BY 1;
SELECT a+1, count(*) FROM grouppos GROUP BY 1 ORDER BY 1;
SELECT a, b, count(*) FROM grouppos GROUP BY 1,2 ORDER BY 1,2;
SELECT a, b, count(*) FROM grouppos GROUP BY 1,b ORDER BY 1,2;
SELECT a, count(*) FROM grouppos GROUP BY 1 HAVING count(*) > 1 ORDER BY 1;
SELECT a, sum(c) FROM grouppos GROUP BY 1 ORDER BY 1;
SELECT 1, count(*) FROM grouppos GROUP BY 1;
SELECT a FROM (SELECT a, count(*) FROM grouppos GROUP BY 1) t ORDER BY a;

-- including inside the grouping-set spellings
SELECT a, b, count(*) FROM grouppos GROUP BY ROLLUP(1,2) ORDER BY 1,2,3;
SELECT a, b, count(*) FROM grouppos GROUP BY CUBE(1,2) ORDER BY 1,2,3;
SELECT a, b, count(*) FROM grouppos GROUP BY GROUPING SETS ((1),(2)) ORDER BY 1,2,3;

-- out of range, in PostgreSQL's words; and a bare integer only — `1+0` is a
-- constant expression, not a position
SELECT a, count(*) FROM grouppos GROUP BY 5;
SELECT a, count(*) FROM grouppos GROUP BY 0;
SELECT a, count(*) FROM grouppos GROUP BY 1+0;

-- the ungrouped-column error names the column, qualified by its table
SELECT c FROM grouppos GROUP BY a;
SELECT b, c FROM grouppos GROUP BY a;
SELECT grouppos.c FROM grouppos GROUP BY a;
SELECT c+1 FROM grouppos GROUP BY a;
SELECT upper(b) FROM grouppos GROUP BY a;
SELECT CASE WHEN c > 1 THEN 1 END FROM grouppos GROUP BY a;
SELECT c FROM grouppos GROUP BY a, b;
SELECT t.c FROM grouppos t GROUP BY t.a;

-- ORDER BY positions still resolve the way they did
SELECT a, count(*) FROM grouppos GROUP BY a ORDER BY 1;
SELECT a, count(*) FROM grouppos GROUP BY a ORDER BY 5;

DROP TABLE grouppos;
