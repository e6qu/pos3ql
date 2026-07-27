-- SELECT ... INTO table — the older spelling of CREATE TABLE AS, matching
-- PostgreSQL 18. The query's result is materialized into a new table; the
-- INTO clause is only legal at the top level.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS si_src;
DROP TABLE IF EXISTS si_a;
DROP TABLE IF EXISTS si_b;
DROP TABLE IF EXISTS si_c;
DROP TABLE IF EXISTS si_d;
DROP TABLE IF EXISTS si_e;

CREATE TABLE si_src (id int, v int, s text);
INSERT INTO si_src VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c');

-- Basic projection with a WHERE.
SELECT id, v INTO si_a FROM si_src WHERE id < 3;
SELECT id, v FROM si_a ORDER BY id;

-- INTO TABLE, computed columns with aliases, and a trailing ORDER BY.
SELECT id AS k, v * 2 AS d INTO TABLE si_b FROM si_src ORDER BY id;
SELECT k, d FROM si_b ORDER BY k;

-- SELECT * materializes every column.
SELECT * INTO si_c FROM si_src WHERE id = 2;
SELECT id, v, s FROM si_c;

-- A grouped/aggregated query.
SELECT s, count(*) AS n INTO si_d FROM si_src GROUP BY s ORDER BY s;
SELECT s, n FROM si_d ORDER BY s;

-- A set operation carries INTO on its first branch, applying to the whole.
SELECT id INTO si_e FROM si_src WHERE id = 1
  UNION SELECT id FROM si_src WHERE id = 3;
SELECT id FROM si_e ORDER BY id;

-- Re-running into an existing table is an error (42P07).
SELECT id INTO si_a FROM si_src;

-- INTO is not allowed in a subquery (42601).
SELECT * FROM (SELECT 1 INTO nope) x;

DROP TABLE si_src;
DROP TABLE si_a;
DROP TABLE si_b;
DROP TABLE si_c;
DROP TABLE si_d;
DROP TABLE si_e;
