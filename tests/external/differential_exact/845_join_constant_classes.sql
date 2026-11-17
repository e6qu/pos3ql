-- A join column equated with a constant puts the constant in its
-- equivalence class: every member is restricted to it after its relation's
-- own conditions (generate_base_implied_equalities_const), the class joins
-- nothing, its relations still join first (have_relevant_eclass_joinclause),
-- and only the written clauses keep its columns above the scans.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/equivclass.c
CREATE TABLE ea845 (id int, x int, t text);
CREATE TABLE eb845 (id int, y int);
CREATE TABLE ec845 (k int, z int);
INSERT INTO ea845 SELECT g, g % 10, 'a' || g FROM generate_series(1, 1000) g;
INSERT INTO eb845 SELECT g % 500, g FROM generate_series(1, 1500) g;
INSERT INTO ec845 SELECT g % 50, g FROM generate_series(1, 200) g;
CREATE INDEX eb845_id ON eb845 (id);
ANALYZE ea845; ANALYZE eb845; ANALYZE ec845;
EXPLAIN SELECT * FROM ea845 JOIN eb845 ON ea845.id = eb845.id WHERE ea845.id = 7;
EXPLAIN SELECT * FROM ea845 JOIN eb845 ON ea845.id = eb845.id WHERE eb845.id = 7 AND ea845.x < 5;
EXPLAIN SELECT * FROM ea845 JOIN eb845 ON ea845.id = eb845.id
  JOIN ec845 ON ec845.k = eb845.id WHERE ec845.k = 3;
EXPLAIN SELECT ea845.t, eb845.y, ec845.z FROM ea845 JOIN eb845 ON ea845.id = eb845.id
  JOIN ec845 ON ec845.k = ea845.id WHERE ea845.id = 3;
EXPLAIN SELECT * FROM ea845 JOIN eb845 ON ea845.id = eb845.id
  JOIN ec845 ON ec845.z = eb845.y WHERE ea845.id = 3;
SELECT * FROM ea845 JOIN eb845 ON ea845.id = eb845.id
  JOIN ec845 ON ec845.k = eb845.id WHERE ec845.k = 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT * FROM ea845 JOIN eb845 ON ea845.id = eb845.id
  JOIN ec845 ON ec845.k = eb845.id WHERE ec845.k = 3;
