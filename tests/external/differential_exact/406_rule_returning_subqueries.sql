-- Subqueries in a RETURNING list routed through a rule's RETURNING list:
-- each subquery's own FROM items bind the names they provide, and other
-- names read the rule's relation, the statement's FROM or USING items, or
-- the action's old and new images. Unqualified names inside subqueries of
-- writes through automatically updatable views resolve the same way.
CREATE SCHEMA rule_returning_subqueries;
SET search_path = rule_returning_subqueries, public;
CREATE TABLE base (id int PRIMARY KEY, label text);
INSERT INTO base VALUES (1, 'one'), (2, 'two');
CREATE VIEW v AS SELECT id, label FROM base;
CREATE RULE vu AS ON UPDATE TO v DO INSTEAD UPDATE base SET label = new.label WHERE id = old.id RETURNING base.*;
CREATE RULE vd AS ON DELETE TO v DO INSTEAD DELETE FROM base WHERE id = old.id RETURNING base.*;
CREATE RULE vi AS ON INSERT TO v DO INSTEAD INSERT INTO base VALUES (new.id, new.label) RETURNING base.*;
CREATE TABLE o (id int, label text, extra text);
INSERT INTO o VALUES (1, 'uno', 'a'), (2, 'dos', 'b');
CREATE TABLE notes (id int, note text);
INSERT INTO notes VALUES (1, 'first'), (2, 'second'), (2, 'again');
UPDATE v SET label = 'x' WHERE id = 1 RETURNING id, (SELECT count(*) FROM notes WHERE notes.id = v.id) AS notes;
UPDATE v SET label = 'y' WHERE id = 2 RETURNING (SELECT string_agg(note, ',' ORDER BY note) FROM notes WHERE id = v.id), (SELECT label FROM o WHERE o.id = 1), (SELECT label);
UPDATE v SET label = o.label FROM o WHERE v.id = o.id RETURNING v.id, (SELECT o.extra || '/' || label FROM notes WHERE notes.id = o.id LIMIT 1);
UPDATE v SET label = 'z' FROM o WHERE v.id = o.id AND o.id = 1 RETURNING (SELECT max(note) FROM notes n WHERE n.id = v.id), EXISTS (SELECT 1 FROM o o2 WHERE o2.extra = o.extra), o.id IN (SELECT id FROM notes);
DELETE FROM v USING o WHERE v.id = o.id AND o.id = 2 RETURNING (SELECT array_agg(note ORDER BY note) FROM notes WHERE id = v.id), (SELECT extra), old.label, new.label, old, new;
INSERT INTO v VALUES (3, 'three') RETURNING (SELECT count(*) FROM base), (SELECT label FROM o WHERE id = 1) AS o_label, (SELECT v.label || label FROM o WHERE id = 1);
UPDATE v SET label = 'q' WHERE id = 3 RETURNING (SELECT nosuch FROM notes);
UPDATE v SET label = 'q' WHERE id = 3 RETURNING (SELECT v.nosuch FROM notes);
SELECT * FROM base ORDER BY id;
-- An unqualified name inside a subquery is that subquery's column first.
CREATE TABLE b2 (id int, x text);
INSERT INTO b2 VALUES (1, 'b');
CREATE VIEW vv AS SELECT id, x AS label FROM b2;
UPDATE vv SET label = (SELECT label FROM o WHERE o.id = vv.id) RETURNING label, (SELECT label FROM o WHERE o.id = 2), (SELECT vv.label FROM o WHERE o.id = 2);
UPDATE vv SET label = 'q' WHERE id IN (SELECT id FROM o WHERE label = 'uno') RETURNING *;
DELETE FROM vv WHERE EXISTS (SELECT 1 FROM o WHERE label = 'dos' AND o.id = vv.id + 1) RETURNING *;
-- old and new read the action's images of its entries.
CREATE TABLE ib (id int PRIMARY KEY, label text);
INSERT INTO ib VALUES (1, 'one');
CREATE VIEW iv AS SELECT id, label FROM ib;
CREATE RULE vu AS ON UPDATE TO iv DO INSTEAD UPDATE ib SET label = new.label WHERE id = old.id RETURNING ib.*;
UPDATE iv SET label = 'x' RETURNING old.label, new.label, old, new;
CREATE RULE vd AS ON DELETE TO iv DO INSTEAD DELETE FROM ib WHERE id = old.id RETURNING ib.id, ib.label || '!';
DELETE FROM iv RETURNING old.label, new.label, old, new;
DROP SCHEMA rule_returning_subqueries CASCADE;
