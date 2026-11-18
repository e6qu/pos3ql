-- Join removal (remove_useless_joins): a left join to one relation that
-- no row above reads, which a unique index's keys, each equated with the
-- join's left side or a constant, make match at most one row, is dropped
-- with its conditions; a column only the dropped join read stays in its
-- relation's scan width, and a query left with one relation scans it with
-- the query's own output width. Joins whose relation is read above, or
-- not proven unique, stay.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/analyzejoins.c
CREATE TABLE ra851 (id int, k int, v int, w text);
CREATE TABLE rb851 (id int, k int, v int, w text);
CREATE TABLE rc851 (id int, k int, v int, w text);
INSERT INTO ra851 SELECT g, g % 50, g % 7, 'a' || g FROM generate_series(1, 400) g;
INSERT INTO rb851 SELECT g, g % 300, g % 11, 'b' || g FROM generate_series(1, 1200) g;
INSERT INTO rc851 SELECT g, g % 40, g / 40, 'c' || g FROM generate_series(1, 300) g;
CREATE UNIQUE INDEX rb851_id ON rb851 (id);
CREATE UNIQUE INDEX rc851_kv ON rc851 (k, v);
ANALYZE ra851; ANALYZE rb851; ANALYZE rc851;
EXPLAIN SELECT ra851.id, ra851.w FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id;
SELECT count(*), sum(ra851.id) FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id;
EXPLAIN SELECT ra851.id, rc851.id FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id JOIN rc851 ON ra851.v = rc851.v;
EXPLAIN SELECT ra851.id FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id AND rb851.v < 3 AND ra851.v > 2;
EXPLAIN SELECT ra851.id FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.k;
EXPLAIN SELECT ra851.id, rb851.id FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id;
EXPLAIN SELECT ra851.id FROM ra851 LEFT JOIN rc851 ON ra851.k = rc851.k AND rc851.v = 3;
EXPLAIN SELECT ra851.id FROM ra851 LEFT JOIN rc851 ON ra851.k = rc851.k AND ra851.v = rc851.v WHERE ra851.id < 30;
EXPLAIN SELECT ra851.id FROM ra851 LEFT JOIN rc851 ON ra851.k = rc851.k;
EXPLAIN SELECT ra851.id FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id LEFT JOIN rc851 ON rb851.k = rc851.k AND rb851.v = rc851.v;
EXPLAIN SELECT rc851.id, ra851.id FROM rc851 LEFT JOIN ra851 ON rc851.k = ra851.k LEFT JOIN rb851 ON ra851.id = rb851.id WHERE rc851.id < 50;
SELECT rc851.id, ra851.id FROM rc851 LEFT JOIN ra851 ON rc851.k = ra851.k LEFT JOIN rb851 ON ra851.id = rb851.id WHERE rc851.id < 5 ORDER BY 1, 2;
EXPLAIN SELECT ra851.id FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id WHERE rb851.id IS NULL;
SELECT ra851.id FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id LEFT JOIN rc851 ON rb851.k = rc851.k AND rb851.v = rc851.v WHERE ra851.id < 8 ORDER BY 1;
EXPLAIN SELECT ra851.id FROM ra851 LEFT JOIN rc851 ON ra851.k = rc851.k AND rc851.v = 3;
SELECT ra851.id, ra851.w FROM ra851 LEFT JOIN rb851 ON ra851.k = rb851.id WHERE ra851.id < 6 ORDER BY 1;
