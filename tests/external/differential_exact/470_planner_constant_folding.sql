-- The planner folds each constant subexpression once, before any row is
-- read: a notice its evaluation raises is sent once, however many rows the
-- query returns, even when no row reaches it. Only immutable calls fold; a
-- strict routine with a constant NULL input folds to NULL without running,
-- and a folding error is the statement's error even in an unreached arm.
CREATE TABLE cf470 (a int);
INSERT INTO cf470 VALUES (1), (2), (3);
SELECT a, ''::tsquery FROM cf470;
SELECT a FROM cf470 WHERE a > 0 OR ''::tsquery IS NULL;
SELECT CASE WHEN a > 5 THEN ''::tsquery END FROM cf470;
SELECT a, numnode(''::tsquery) FROM cf470 WHERE false;
SELECT CASE WHEN a > 5 THEN 1 / 0 END FROM cf470;
SELECT CASE WHEN false THEN 1 / 0 ELSE a END FROM cf470;
SELECT CASE 1 WHEN 2 THEN 1 / 0 ELSE a END FROM cf470;
SELECT a FROM cf470 WHERE false AND 1 / 0 = 1;
SELECT a FROM cf470 WHERE 1 / 0 = 1 AND false;
-- to_tsquery(text) reads default_text_search_config: it is stable and runs
-- for each row, where to_tsquery(regconfig, text) is immutable and folds.
SELECT a, to_tsquery('') FROM cf470;
SELECT a, to_tsquery('english', '') FROM cf470;
SELECT a, plainto_tsquery('the') FROM cf470;
SELECT a FROM cf470 WHERE a = ANY (ARRAY[1, numnode(''::tsquery)]);
SELECT a, (SELECT ''::tsquery) FROM cf470;
SELECT a, upper('x') FROM cf470 GROUP BY a, upper('x') ORDER BY upper('x'), a;
CREATE FUNCTION cf470_note(int) RETURNS int IMMUTABLE LANGUAGE plpgsql
  AS $$BEGIN RAISE NOTICE 'cf470_note %', $1; RETURN $1; END$$;
CREATE FUNCTION cf470_strict(int) RETURNS int STRICT IMMUTABLE
  LANGUAGE plpgsql AS $$BEGIN RAISE NOTICE 'cf470_strict %', $1; RETURN $1; END$$;
CREATE FUNCTION cf470_stable(int) RETURNS int STABLE LANGUAGE plpgsql
  AS $$BEGIN RAISE NOTICE 'cf470_stable %', $1; RETURN $1; END$$;
CREATE FUNCTION cf470_sql(int) RETURNS int IMMUTABLE LANGUAGE sql
  AS $$SELECT cf470_note($1) + 1$$;
SELECT a, cf470_note(7) FROM cf470;
SELECT a FROM cf470 WHERE a > cf470_note(0) ORDER BY a;
SELECT CASE WHEN a > 100 THEN cf470_note(8) END FROM cf470;
SELECT a, cf470_note(a) FROM cf470;
SELECT a, cf470_strict(NULL) FROM cf470;
SELECT a, cf470_strict(cf470_note(NULL)) FROM cf470;
SELECT a, cf470_stable(1) FROM cf470;
SELECT a, cf470_sql(3) FROM cf470;
SELECT cf470_note(4) FROM cf470 WHERE false;
SELECT cf470_note(5), cf470_note(6);
UPDATE cf470 SET a = a + numnode(''::tsquery) WHERE a < 3;
UPDATE cf470 SET a = a + 0 WHERE a > cf470_note(1) RETURNING a, cf470_note(9);
DELETE FROM cf470 WHERE ''::tsquery IS NULL;
DELETE FROM cf470 WHERE a = cf470_note(10) RETURNING cf470_note(11);
UPDATE cf470 SET a = 1 / 0 WHERE false;
TABLE cf470;
