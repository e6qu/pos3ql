-- MERGE, matching PostgreSQL 18. Source-driven: each source row is matched
-- against the target on the ON condition; a match runs the first satisfied WHEN
-- MATCHED clause, a miss the first WHEN NOT MATCHED clause. A target row affected
-- twice is a cardinality violation (21000).
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS mg_tgt;
DROP TABLE IF EXISTS mg_src;

CREATE TABLE mg_tgt (id int PRIMARY KEY, v text, n int);
INSERT INTO mg_tgt VALUES (1,'a',10),(2,'b',20),(3,'c',30);
CREATE TABLE mg_src (id int, v text);
INSERT INTO mg_src VALUES (2,'B'),(3,'C'),(4,'D'),(5,'E');

-- DELETE / UPDATE / DO NOTHING / INSERT, selected by match and AND-condition.
MERGE INTO mg_tgt t
USING mg_src s ON t.id = s.id
WHEN MATCHED AND s.id = 3 THEN DELETE
WHEN MATCHED THEN UPDATE SET v = s.v, n = t.n + 1
WHEN NOT MATCHED AND s.id = 5 THEN DO NOTHING
WHEN NOT MATCHED THEN INSERT (id, v, n) VALUES (s.id, s.v, 0);
SELECT id, v, n FROM mg_tgt ORDER BY id;

-- A matched row with no satisfied WHEN MATCHED clause is left untouched (0).
MERGE INTO mg_tgt t USING mg_src s ON t.id = s.id
  WHEN MATCHED AND false THEN DELETE;
SELECT id, v, n FROM mg_tgt ORDER BY id;

-- Cardinality violation: a target row matched by two source rows.
INSERT INTO mg_src VALUES (2,'dup');
MERGE INTO mg_tgt t USING mg_src s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET v = s.v;

-- A VALUES source with a column alias.
MERGE INTO mg_tgt t USING (VALUES (20,'twenty')) s(id,v) ON t.id = s.id
  WHEN NOT MATCHED THEN INSERT (id, v, n) VALUES (s.id, s.v, 200);
SELECT id, v, n FROM mg_tgt WHERE id = 20;

-- Unqualified columns resolve when unambiguous; ON with an extra predicate.
MERGE INTO mg_tgt t USING mg_src s ON t.id = s.id AND s.v <> t.v
  WHEN MATCHED THEN UPDATE SET v = 'merged';
SELECT id, v FROM mg_tgt WHERE v = 'merged' ORDER BY id;

DROP TABLE mg_tgt;
DROP TABLE mg_src;
