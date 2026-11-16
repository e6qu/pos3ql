-- A condition on an expression no index holds is estimated by PostgreSQL's
-- defaults: an equality 1/DEFAULT_NUM_DISTINCT (1/2 for a boolean), an
-- inequality DEFAULT_INEQ_SEL, two bounds DEFAULT_RANGE_INEQ_SEL, a NULL
-- test DEFAULT_UNK_SEL and a boolean function a third; its operators and
-- functions cost what they call. A cast to a column's own type is no node
-- (printed and costed as the column), and a varchar or text relabeling is
-- the column for its index. An UPDATE or DELETE whose costs choose a
-- sequential scan changes rows in heap order, also when a routine call
-- resumes it. The rows stay under the forced-spill run's 256 KiB memtable.
CREATE TABLE d753 (id int, b text, v varchar, n int, f boolean);
INSERT INTO d753 SELECT g, 'b' || g, 'v' || (g % 100), g % 50, g % 3 = 0
  FROM generate_series(1, 1500) g;
ALTER TABLE d753 ADD PRIMARY KEY (id);
CREATE INDEX ON d753 (v);
CREATE INDEX ON d753 (n);
ANALYZE d753;
\set e 'EXPLAIN'
:e SELECT * FROM d753 WHERE upper(b) = 'X';
:e SELECT * FROM d753 WHERE upper(b) <> 'X';
:e SELECT * FROM d753 WHERE upper(b) > 'X';
:e SELECT * FROM d753 WHERE upper(b) > 'X' AND upper(b) < 'Y';
:e SELECT * FROM d753 WHERE length(b) IS NULL;
:e SELECT * FROM d753 WHERE length(b) IS NOT NULL;
:e SELECT * FROM d753 WHERE n + 1 = 5;
:e SELECT * FROM d753 WHERE (n > 3) = true;
:e SELECT * FROM d753 WHERE upper(b) = NULL;
:e SELECT * FROM d753 WHERE n = 7 AND upper(b) <> 'Q';
:e SELECT * FROM d753 WHERE id::int = 5;
:e SELECT * FROM d753 WHERE n::integer < 2;
:e SELECT * FROM d753 WHERE b::text = 'b7';
:e SELECT * FROM d753 WHERE v::text = 'v7';
:e SELECT * FROM d753 WHERE v::varchar = 'v7';
:e SELECT id::int, v::varchar FROM d753 WHERE n = 3 ORDER BY id::int LIMIT 2;
CREATE VIEW d753_v AS SELECT id::int AS i, v::varchar AS w FROM d753;
SELECT pg_get_viewdef('d753_v');
SELECT * FROM d753 WHERE id::int = 5;
SELECT id FROM d753 WHERE v::text = 'v7' ORDER BY id LIMIT 3;
-- Writes read heap order under a sequential scan.
UPDATE d753 SET b = b || 'u' WHERE id % 37 = 0;
CREATE FUNCTION d753_f(x int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN PERFORM count(*) FROM d753 WHERE id = x; RETURN x + 100; END $$;
:e UPDATE d753 SET n = d753_f(n) WHERE n < 40 AND id > 1400;
WITH u AS (UPDATE d753 SET n = d753_f(n) WHERE n < 40 AND id > 1400 RETURNING id)
  SELECT array_agg(id) FROM u;
WITH u AS (DELETE FROM d753 WHERE n < 48 AND id > 1300 RETURNING id)
  SELECT array_agg(id) FROM u;
