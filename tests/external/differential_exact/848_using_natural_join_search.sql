-- USING and NATURAL joins through PostgreSQL's join search: each joins by
-- the equalities of its named or common columns, and SELECT * reads a
-- merged column's left column, a right join's right one and a full join's
-- both; the top join's width is the query output's
-- (apply_scanjoin_target_to_paths), a column listed twice counted twice.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/parser/parse_clause.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
CREATE TABLE ua848 (id int, x int, t text);
CREATE TABLE ub848 (id int, x int, y int);
CREATE TABLE uc848 (x int, z int);
INSERT INTO ua848 SELECT g, g % 10, 'a' || g FROM generate_series(1, 300) g;
INSERT INTO ub848 SELECT g % 350, g % 7, g FROM generate_series(1, 400) g;
INSERT INTO uc848 SELECT g % 12, g FROM generate_series(1, 50) g;
ANALYZE ua848; ANALYZE ub848; ANALYZE uc848;
EXPLAIN SELECT * FROM ua848 JOIN ub848 USING (id);
SELECT * FROM ua848 JOIN ub848 USING (id) WHERE ua848.id < 20;
EXPLAIN SELECT * FROM ua848 JOIN ub848 USING (id, x);
EXPLAIN SELECT * FROM ua848 NATURAL JOIN ub848;
SELECT * FROM ua848 NATURAL JOIN ub848 WHERE ub848.y < 30;
EXPLAIN SELECT * FROM ua848 LEFT JOIN ub848 USING (id) WHERE ua848.x = 3;
SELECT * FROM ua848 LEFT JOIN ub848 USING (id) WHERE ua848.x = 3;
EXPLAIN SELECT * FROM ua848 RIGHT JOIN ub848 USING (id);
EXPLAIN SELECT * FROM ua848 FULL JOIN ub848 USING (id);
SELECT * FROM ua848 FULL JOIN ub848 USING (id) WHERE ub848.y IS NULL OR ub848.y < 10;
EXPLAIN SELECT ua848.t, uc848.z FROM ua848 JOIN uc848 USING (x);
EXPLAIN SELECT * FROM ua848 JOIN ub848 USING (id) JOIN uc848 ON uc848.x = ub848.x;
SELECT ua848.id, ub848.y, uc848.z FROM ua848 JOIN ub848 USING (id) JOIN uc848 ON uc848.x = ub848.x WHERE ua848.id < 15;
EXPLAIN SELECT ua848.id, ua848.id, ub848.y FROM ua848 JOIN ub848 USING (id);
