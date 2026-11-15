-- A literal's input function raises its notices and errors at the literal's
-- position, as PostgreSQL's parser position callback reports them, whether
-- the literal is cast, assigned, compared or passed to a function, and in a
-- prepared statement at PREPARE, not at each EXECUTE.
CREATE TABLE c570 (id int, q tsquery);
SELECT ''::tsquery;
SELECT 1 WHERE ''::tsquery IS NOT NULL;
INSERT INTO c570 VALUES (1, '');
UPDATE c570 SET q = '' WHERE id = 1;
SELECT count(*) FROM c570 WHERE q = '';
SELECT q || '' FROM c570;
SELECT numnode('');
SELECT querytree('');
SELECT ts_rank('a'::tsvector, '');
SELECT '{"",a}'::tsquery[];
SELECT CASE WHEN true THEN ''::tsquery END;
SELECT tsquery '';
SELECT CAST('' AS tsquery);
SELECT * FROM (VALUES (''::tsquery)) v;
SELECT 'abc'::int;
SELECT 1 + 'abc';
SELECT '{1,x}'::int[];
SELECT '2020-13-01'::date;
PREPARE c570_select AS SELECT ''::tsquery, 1 WHERE 2 = 2;
EXECUTE c570_select;
EXECUTE c570_select;
PREPARE c570_insert AS INSERT INTO c570 VALUES (2, '');
EXECUTE c570_insert;
EXECUTE c570_insert;
PREPARE c570_compare AS SELECT 2 = 'abc';
PREPARE c570_fromless AS SELECT 1 WHERE 2 = 'abc';
PREPARE c570_fromless_left AS SELECT 1 WHERE 'abc' = 2;
SELECT count(*) FROM c570;
