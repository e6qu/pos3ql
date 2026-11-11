-- Data-modifying WITH queries: one snapshot for every sub-statement, the
-- unread ones run after the main query (last written first), and each runs
-- exactly once even when the main query calls PL/pgSQL functions.
CREATE TABLE dw86 (a int);
CREATE TABLE dw86_log (msg text);
CREATE FUNCTION dw86_count() RETURNS bigint LANGUAGE plpgsql VOLATILE AS $$
BEGIN RETURN (SELECT count(*) FROM dw86); END $$;
CREATE FUNCTION dw86_note(t text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN INSERT INTO dw86_log VALUES (t); RETURN 1; END $$;
WITH w AS (INSERT INTO dw86 VALUES (1)) SELECT dw86_count();
SELECT count(*) FROM dw86;
WITH w AS (INSERT INTO dw86 VALUES (2) RETURNING a)
SELECT dw86_count(), (SELECT count(*) FROM dw86), (SELECT a FROM w);
WITH w AS (DELETE FROM dw86 RETURNING a)
SELECT count(*), (SELECT count(*) FROM dw86) FROM w;
INSERT INTO dw86 VALUES (5);
WITH a AS (UPDATE dw86 SET a = a + 1 RETURNING a),
     b AS (UPDATE dw86 SET a = a + 10 RETURNING a)
SELECT (SELECT array_agg(a) FROM a), (SELECT array_agg(a) FROM b);
SELECT * FROM dw86;
WITH a AS (INSERT INTO dw86 VALUES (7) RETURNING a), b AS (SELECT count(*) FROM dw86)
SELECT * FROM b;
WITH a AS (INSERT INTO dw86 VALUES (8)) INSERT INTO dw86 SELECT dw86_count() + 100;
SELECT * FROM dw86 ORDER BY 1;
WITH a AS (INSERT INTO dw86 VALUES (dw86_note('cte'))) SELECT dw86_note('main');
SELECT * FROM dw86_log;
DELETE FROM dw86_log;
WITH a AS (INSERT INTO dw86 VALUES (dw86_note('cte1'))),
     b AS (INSERT INTO dw86 VALUES (dw86_note('cte2')))
SELECT dw86_note('main');
SELECT * FROM dw86_log;
DELETE FROM dw86;
INSERT INTO dw86 VALUES (1), (2);
WITH d AS (DELETE FROM dw86) INSERT INTO dw86 VALUES (99);
SELECT * FROM dw86 ORDER BY 1;
WITH u AS (UPDATE dw86 SET a = a + 1) INSERT INTO dw86 SELECT a * 10 FROM dw86;
SELECT * FROM dw86 ORDER BY 1;
WITH i AS (INSERT INTO dw86 VALUES (7)) UPDATE dw86 SET a = -a WHERE a = 7;
SELECT * FROM dw86 ORDER BY 1;
BEGIN;
INSERT INTO dw86 VALUES (50);
WITH d AS (DELETE FROM dw86 WHERE a = 50 RETURNING a) SELECT * FROM d;
WITH d AS (DELETE FROM dw86 WHERE a = 7) SELECT count(*) FROM dw86;
COMMIT;
SELECT * FROM dw86 ORDER BY 1;
WITH x AS (INSERT INTO dw86 VALUES (1/0)) SELECT 1;
WITH x AS (INSERT INTO dw86 VALUES (3)) SELECT 1/0;
SELECT * FROM dw86 ORDER BY 1;
