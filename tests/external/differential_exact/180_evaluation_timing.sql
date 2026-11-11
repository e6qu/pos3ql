-- A materialized WITH query runs when its rows are first read, and an IN,
-- ANY or ALL subquery with effects runs when evaluation reaches it, once per
-- statement unless it is correlated.
CREATE SEQUENCE s180;
CREATE TABLE log180(n int, what text);
CREATE FUNCTION note180(t text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN INSERT INTO log180 VALUES (nextval('s180'), t); RETURN 1; END $$;
WITH c AS MATERIALIZED (SELECT note180('cte') a) SELECT note180('main'), (SELECT a FROM c);
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
WITH c AS MATERIALIZED (SELECT note180('cte') a) SELECT note180('main') WHERE false AND EXISTS (SELECT 1 FROM c);
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
WITH c AS (SELECT note180('cte') a) SELECT 1 FROM (SELECT note180('outer')) o, c;
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
WITH c AS MATERIALIZED (SELECT 1 / 0 a) SELECT 1 WHERE false AND 1 IN (SELECT a FROM c);
WITH c AS MATERIALIZED (SELECT note180('cte') a) SELECT 1 FROM generate_series(1, 0) g WHERE g IN (SELECT a FROM c);
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
SELECT CASE WHEN false THEN 1 IN (SELECT note180('in')) ELSE true END;
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
SELECT note180('first') = 1 AND 1 IN (SELECT note180('second'));
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
SELECT x, x IN (SELECT note180('in' || x)) FROM generate_series(1, 3) x;
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
SELECT x, x = ANY (SELECT note180('any') + g FROM generate_series(0, 2) g) FROM generate_series(1, 3) x;
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
SELECT 1 = ANY (SELECT note180('any')) WHERE false;
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
SELECT CASE WHEN x > 1 THEN x = ALL (SELECT note180('all')) END FROM generate_series(1, 3) x;
SELECT what FROM log180 ORDER BY n; TRUNCATE log180;
DROP FUNCTION note180; DROP TABLE log180; DROP SEQUENCE s180;
