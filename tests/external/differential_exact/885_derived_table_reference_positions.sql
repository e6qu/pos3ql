-- A reference to a derived table's own relation from outside it fails at
-- the outer reference, as parse analysis reaches it: a lookup the derived
-- table's subquery answers from an enclosing level leaves no position.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/parser/parse_relation.c
CREATE TABLE ra885 (id int, k int);
CREATE TABLE rb885 (id int, k int);
SELECT sum(ra885.id) FROM (SELECT ra885.id FROM ra885 WHERE EXISTS (SELECT 1 FROM rb885 WHERE rb885.k = ra885.id)) s;
SELECT sum(ra885.id) FROM (SELECT ra885.id FROM ra885 WHERE ra885.id > 0) s;
SELECT ra885.id FROM (SELECT ra885.id FROM ra885 WHERE EXISTS (SELECT 1 FROM rb885 WHERE rb885.k = ra885.id)) s;
SELECT s.id FROM (SELECT ra885.id FROM ra885 WHERE EXISTS (SELECT 1 FROM rb885 WHERE rb885.k = ra885.id)) s WHERE ra885.k = 1;
SELECT s.id FROM (SELECT ra885.id FROM ra885 WHERE ra885.k IN (SELECT rb885.k FROM rb885 WHERE rb885.id = ra885.id)) s ORDER BY ra885.id;
SELECT s.id, (SELECT count(*) FROM rb885 WHERE rb885.k = s.id) FROM (SELECT ra885.id FROM ra885) s WHERE rb885.id = 1;
