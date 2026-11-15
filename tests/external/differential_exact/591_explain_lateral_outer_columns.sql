-- EXPLAIN plans a LATERAL item, and a subquery's derived table, with the
-- columns of the FROM items to its left and of the enclosing queries in
-- scope. A LATERAL subquery runs again for each outer row with its
-- parameters, so a nested loop keeps no Materialize copy of it, and its
-- conditions name the outer relation's columns qualified.
CREATE TABLE a591 (id int, v int);
CREATE TABLE b591 (id int, w int);
INSERT INTO a591 VALUES (1, 10), (2, 20);
INSERT INTO b591 VALUES (1, 5), (1, 6), (2, 7);
EXPLAIN (COSTS OFF) SELECT * FROM a591, LATERAL (SELECT a591.v + 1 AS z OFFSET 0) s;
EXPLAIN (COSTS OFF) SELECT * FROM a591, LATERAL (SELECT count(*) AS c FROM b591 WHERE b591.id = a591.id) s;
EXPLAIN (COSTS OFF) SELECT * FROM a591, LATERAL (SELECT * FROM (SELECT w FROM b591 WHERE b591.id = a591.id) d OFFSET 0) s;
EXPLAIN (COSTS OFF) SELECT * FROM a591, LATERAL (SELECT w FROM b591 WHERE b591.w > a591.v / 3 ORDER BY w LIMIT 1) s;
EXPLAIN (COSTS OFF) SELECT * FROM a591, LATERAL generate_series(1, a591.id) g;
SELECT * FROM a591, LATERAL (SELECT w FROM b591 WHERE b591.w > a591.v / 3 ORDER BY w LIMIT 1) s ORDER BY 1;
SELECT * FROM a591, LATERAL (SELECT a591.v + 1 AS z OFFSET 0) s ORDER BY 1;
