-- An equivalence class holding two columns of one relation: the relation
-- is restricted by their equality, each member equated with the previous
-- one of its relation (generate_base_implied_equalities_no_const), estimated
-- at the default equality selectivity, and a join takes the first member
-- of each side; an index on either column derives its clause from that
-- column's own member (generate_implied_equalities_for_column).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/equivclass.c
CREATE TABLE sa877 (id int, k int, v int, w text);
CREATE TABLE sb877 (id int, k int, v int, w text);
CREATE TABLE sc877 (id int, k int, v int);
INSERT INTO sa877 SELECT g, g % 23, g % 19, 'a' || (g % 5) FROM generate_series(1, 500) g;
INSERT INTO sb877 SELECT g, g % 29, g % 7, 'b' || (g % 3) FROM generate_series(1, 300) g;
INSERT INTO sc877 SELECT g, g % 31, g % 11 FROM generate_series(1, 200) g;
CREATE INDEX sa877_v ON sa877 (v);
CREATE INDEX sb877_k ON sb877 (k);
ANALYZE sa877; ANALYZE sb877; ANALYZE sc877;
EXPLAIN SELECT sa877.id, sb877.id FROM sa877 JOIN sb877 ON sa877.k = sb877.k AND sa877.v = sb877.k;
SELECT sa877.id, sb877.id FROM sa877 JOIN sb877 ON sa877.k = sb877.k AND sa877.v = sb877.k ORDER BY 1, 2 LIMIT 20;
EXPLAIN SELECT sa877.id, sb877.id FROM sa877 JOIN sb877 ON sb877.k = sa877.v AND sa877.k = sb877.k WHERE sb877.v < 3;
EXPLAIN SELECT sa877.id, sb877.id, sc877.id FROM sa877 JOIN sb877 ON sa877.k = sb877.k JOIN sc877 ON sc877.k = sb877.k AND sc877.v = sa877.k;
SELECT sa877.id, sb877.id, sc877.id FROM sa877 JOIN sb877 ON sa877.k = sb877.k JOIN sc877 ON sc877.k = sb877.k AND sc877.v = sa877.k ORDER BY 1, 2, 3 LIMIT 20;
EXPLAIN SELECT sa877.id FROM sa877 WHERE EXISTS (SELECT 1 FROM sb877 WHERE sb877.k = sa877.k AND sb877.v = sa877.k);
SELECT sa877.id FROM sa877 WHERE EXISTS (SELECT 1 FROM sb877 WHERE sb877.k = sa877.k AND sb877.v = sa877.k) ORDER BY 1 LIMIT 10;
