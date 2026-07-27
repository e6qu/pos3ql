-- COPY (query) TO STDOUT — streaming a query's result rows in COPY's text /
-- CSV formats, matching PostgreSQL 18. Distinctive names, dropped up front.
DROP TABLE IF EXISTS cq;

CREATE TABLE cq (id int, v int, s text);
INSERT INTO cq VALUES (1, 10, 'a'), (2, 20, 'b,x'), (3, NULL, 'c "q"');

-- Default (tab-delimited) text format, with a projection and ORDER BY.
COPY (SELECT id, s FROM cq ORDER BY id) TO STDOUT;

-- CSV with a header line; embedded commas and quotes get CSV quoting, and a
-- NULL renders as the empty field.
COPY (SELECT * FROM cq ORDER BY id) TO STDOUT WITH CSV HEADER;

-- A custom NULL marker and an explicit delimiter.
COPY (SELECT id, v FROM cq ORDER BY id) TO STDOUT (FORMAT csv, NULL 'NUL');
COPY (SELECT id, s FROM cq WHERE id > 1 ORDER BY id) TO STDOUT WITH DELIMITER '|';

-- FORCE_QUOTE names an output column and quotes every value of it.
COPY (SELECT id, s FROM cq ORDER BY id) TO STDOUT (FORMAT csv, FORCE_QUOTE (s));

-- The query can aggregate / group / join like any SELECT.
COPY (SELECT count(*) AS n, sum(v) AS total FROM cq) TO STDOUT WITH CSV HEADER;
COPY (SELECT s, count(*) AS c FROM cq GROUP BY s ORDER BY s) TO STDOUT WITH CSV;

-- Only TO STDOUT is accepted for a query source (a query cannot be COPY FROM).
COPY (SELECT id FROM cq) FROM STDIN;

DROP TABLE cq;
