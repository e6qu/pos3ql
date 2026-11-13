-- A RETURNING list routed through a rule's RETURNING list reads the rule's
-- relation from the action's entries and the statement's FROM or USING
-- items from their rows: their columns, whole rows and `*`.
CREATE SCHEMA rule_returning_sources;
SET search_path = rule_returning_sources, public;
CREATE TABLE base (id int PRIMARY KEY, label text);
INSERT INTO base VALUES (1, 'one'), (2, 'two');
CREATE VIEW v AS SELECT id, label FROM base;
CREATE RULE vu AS ON UPDATE TO v DO INSTEAD UPDATE base SET label = new.label WHERE id = old.id RETURNING base.*;
CREATE RULE vd AS ON DELETE TO v DO INSTEAD DELETE FROM base WHERE id = old.id RETURNING base.*;
CREATE RULE vi AS ON INSERT TO v DO INSTEAD INSERT INTO base VALUES (new.id, new.label) RETURNING base.*;
CREATE TABLE o (id int, label text, extra text);
INSERT INTO o VALUES (1, 'uno', 'a'), (2, 'dos', 'b');
UPDATE v SET label = o.label FROM o WHERE v.id = o.id RETURNING o.label;
UPDATE v SET label = o.label || '!' FROM o WHERE v.id = o.id RETURNING *;
UPDATE v SET label = 'x' FROM o WHERE v.id = o.id AND o.id = 1 RETURNING v.*, o.*;
UPDATE v SET label = 'z' FROM o WHERE v.id = o.id AND o.id = 2 RETURNING v.label, o.id + v.id, extra;
UPDATE v AS w SET label = 'w' FROM o WHERE w.id = o.id AND o.id = 1 RETURNING w.id, o.label, o;
UPDATE v SET label = 'q' FROM o WHERE v.id = o.id RETURNING label;
UPDATE v SET label = 'q' FROM o WHERE v.id = o.id RETURNING o.nope;
UPDATE v SET label = 'q' FROM o WHERE v.id = o.id RETURNING nope;
UPDATE v SET label = 'q' FROM o, (SELECT 1 AS k) s WHERE v.id = o.id AND s.k = 1 RETURNING s.k, o.id, s;
DELETE FROM v USING o WHERE v.id = o.id AND o.id = 2 RETURNING o.*, v.label;
SELECT * FROM base ORDER BY id;
DROP SCHEMA rule_returning_sources CASCADE;
