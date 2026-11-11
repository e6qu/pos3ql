-- A data-modifying WITH query runs when the statement first reads its rows,
-- and a subquery with effects runs where evaluation reaches it, so calls,
-- notices, sequence values and visibility interleave as in PostgreSQL.
CREATE TABLE wq110 (a int);
CREATE TABLE wq110_log (msg text);
CREATE SEQUENCE wq110_seq;
CREATE FUNCTION wq110_note(t text) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN RAISE NOTICE 'note %', t; INSERT INTO wq110_log VALUES (t); RETURN 1; END $$;
CREATE FUNCTION wq110_count() RETURNS bigint LANGUAGE plpgsql VOLATILE AS $$
BEGIN RETURN (SELECT count(*) FROM wq110); END $$;
WITH w AS (INSERT INTO wq110 VALUES (1) RETURNING a)
SELECT wq110_count(), (SELECT a FROM w), wq110_count();
WITH w AS (INSERT INTO wq110 VALUES (wq110_note('cte')) RETURNING a)
SELECT wq110_note('before'), (SELECT a FROM w), wq110_note('after');
WITH w AS (INSERT INTO wq110 VALUES (wq110_note('cte2')) RETURNING a)
SELECT wq110_note('row') FROM w;
WITH w AS (INSERT INTO wq110 VALUES (wq110_note('cte3')) RETURNING a)
SELECT wq110_note('pair') FROM generate_series(1, 2) g, w;
WITH w AS (INSERT INTO wq110 VALUES (wq110_note('unread')) RETURNING a)
SELECT CASE WHEN wq110_note('test') = 0 THEN (SELECT a FROM w) END;
WITH w AS (INSERT INTO wq110 VALUES (nextval('wq110_seq')) RETURNING a)
SELECT nextval('wq110_seq'), (SELECT a FROM w), nextval('wq110_seq');
WITH a AS (INSERT INTO wq110 VALUES (wq110_note('a')) RETURNING a),
     b AS (INSERT INTO wq110 VALUES (wq110_note('b')) RETURNING a)
SELECT (SELECT a FROM b), (SELECT a FROM a);
WITH a AS (INSERT INTO wq110 VALUES (wq110_note('inner')) RETURNING a),
     b AS (INSERT INTO wq110 SELECT a + wq110_note('outer') FROM a RETURNING a)
SELECT wq110_note('main'), (SELECT a FROM b), (SELECT a FROM a);
WITH a AS (INSERT INTO wq110 VALUES (wq110_note('first')) RETURNING a),
     b AS (INSERT INTO wq110 VALUES (wq110_note('second')) RETURNING a)
SELECT wq110_note('neither');
WITH w AS (INSERT INTO wq110 VALUES (wq110_note('source')) RETURNING a)
INSERT INTO wq110 SELECT a + wq110_note('target') FROM w;
SELECT msg, count(*) FROM wq110_log GROUP BY msg ORDER BY msg;
SELECT count(*) FROM wq110;
SELECT nextval('wq110_seq');
-- Subqueries with effects run where they are reached, uncorrelated ones once.
SELECT wq110_note('x'), (SELECT wq110_note('y'));
SELECT wq110_note('x2'), (SELECT wq110_note('y2')) FROM generate_series(1, 2);
SELECT wq110_note('x3'), (SELECT wq110_note('y3') FROM generate_series(1, 1) g WHERE g.g = s.s)
FROM generate_series(1, 2) s;
SELECT CASE WHEN wq110_note('x4') = 2 THEN (SELECT wq110_note('y4')) END;
SELECT nextval('wq110_seq'), (SELECT nextval('wq110_seq')), nextval('wq110_seq');
-- A statement's reads use its snapshot, not rows a routine it calls adds.
CREATE FUNCTION wq110_add() RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN INSERT INTO wq110 VALUES (1000); RETURN 1; END $$;
DELETE FROM wq110;
INSERT INTO wq110 VALUES (5);
INSERT INTO wq110 SELECT wq110_add() + count(*) * 10 FROM wq110;
SELECT * FROM wq110 ORDER BY 1;
-- A scalar subquery is named after its first output column.
SELECT (SELECT * FROM (SELECT 1 AS x) s);
SELECT (SELECT s.* FROM (SELECT 1 AS x) s);
SELECT (SELECT * FROM (SELECT 1) s(z));
WITH c AS (SELECT 5 AS q) SELECT (SELECT * FROM c);
SELECT (SELECT * FROM (SELECT 1 AS x, 2 AS y) s LIMIT 0);
SELECT 1 WHERE 1 = (SELECT * FROM wq110_log);
SELECT 1 FROM wq110 WHERE a IN (SELECT a, a FROM wq110);
SELECT 1 FROM wq110 WHERE a NOT IN (SELECT a, a FROM wq110);
SELECT 1 FROM wq110 WHERE a < ALL (SELECT a, a FROM wq110);
SELECT ARRAY(SELECT a, a FROM wq110);
