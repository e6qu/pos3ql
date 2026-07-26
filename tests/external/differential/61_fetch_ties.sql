-- FETCH FIRST / OFFSET FETCH and WITH TIES, matching PostgreSQL 18. The
-- SQL-standard spelling of LIMIT/OFFSET, plus WITH TIES: after the row limit,
-- also return every row tying with the last on the ORDER BY keys.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS ftt;

CREATE TABLE ftt (id int, grp text, v int);
INSERT INTO ftt VALUES
  (1,'a',10),(2,'a',10),(3,'b',20),(4,'b',20),(5,'c',30),(6,'c',30),(7,'d',40);

-- FETCH FIRST/NEXT [count] ROW/ROWS ONLY (count defaults to 1).
SELECT id FROM ftt ORDER BY id FETCH FIRST 3 ROWS ONLY;
SELECT id FROM ftt ORDER BY id FETCH FIRST ROW ONLY;
SELECT id FROM ftt ORDER BY id FETCH NEXT 2 ROW ONLY;
SELECT id FROM ftt ORDER BY id OFFSET 2 ROWS FETCH NEXT 2 ROWS ONLY;
-- LIMIT and FETCH are interchangeable spellings.
SELECT id FROM ftt ORDER BY id OFFSET 5;

-- WITH TIES: keep rows tying with the last returned on the ORDER BY key.
SELECT id, v FROM ftt ORDER BY v FETCH FIRST 1 ROWS WITH TIES;
SELECT id, v FROM ftt ORDER BY v FETCH FIRST 3 ROWS WITH TIES;
SELECT id, v FROM ftt ORDER BY v FETCH FIRST 4 ROWS WITH TIES;
-- A unique last key returns exactly `count` rows.
SELECT id, v FROM ftt ORDER BY v DESC FETCH FIRST 1 ROWS WITH TIES;
-- WITH TIES composes with OFFSET (ties of the last row in the window).
SELECT id, v FROM ftt ORDER BY v OFFSET 2 ROWS FETCH NEXT 1 ROWS WITH TIES;
-- Multi-key ORDER BY: ties must match on all keys.
SELECT id, grp, v FROM ftt ORDER BY v, grp FETCH FIRST 2 ROWS WITH TIES;

-- WITH TIES over a grouped/aggregate query.
SELECT grp, count(*) FROM ftt GROUP BY grp ORDER BY count(*) DESC FETCH FIRST 1 ROWS WITH TIES;

-- WITH TIES over a UNION ALL (ORDER BY applies to the whole result).
SELECT v FROM ftt UNION ALL SELECT v FROM ftt ORDER BY v FETCH FIRST 1 ROWS WITH TIES;

-- WITH TIES over a FROM-less set-returning function.
SELECT g FROM generate_series(1,5) g ORDER BY g % 2, g FETCH FIRST 1 ROWS WITH TIES;

-- Error: WITH TIES requires ORDER BY (42601).
SELECT id FROM ftt FETCH FIRST 2 ROWS WITH TIES;

DROP TABLE ftt;
