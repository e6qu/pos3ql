-- Correlated subqueries in UPDATE, DELETE and MERGE conditions run for every
-- candidate row; LATERAL set operations read the FROM items to their left.
CREATE SCHEMA correlated_dml;
SET search_path = correlated_dml, public;
CREATE TABLE aa (id int, v int);
CREATE TABLE bb (id int, w int);
INSERT INTO aa VALUES (1, 1), (2, 2), (3, 3);
INSERT INTO bb VALUES (1, 1), (3, 9);
UPDATE aa SET v = v + 10 WHERE EXISTS (SELECT 1 FROM bb WHERE bb.id = aa.id) RETURNING *;
UPDATE aa t SET v = 0 WHERE t.id = (SELECT max(id) FROM bb WHERE bb.w > t.v - 100) RETURNING *;
UPDATE aa SET v = 5 WHERE id IN (SELECT bb.id FROM bb WHERE bb.w = aa.v - 10) RETURNING *;
DELETE FROM aa WHERE EXISTS (SELECT 1 FROM bb WHERE bb.id = aa.id AND bb.w = 9) RETURNING *;
DELETE FROM aa WHERE v = (SELECT aa.v) AND id = 2 RETURNING *;
INSERT INTO aa VALUES (2, 2), (3, 3);
UPDATE aa SET v = (SELECT count(*) FROM bb WHERE bb.id <= aa.id) WHERE id < (SELECT max(id) FROM bb) RETURNING *;
UPDATE aa SET v = 1 FROM bb WHERE bb.id = aa.id AND EXISTS (SELECT 1 FROM bb b2 WHERE b2.w = aa.v) RETURNING aa.*;
DELETE FROM aa USING bb WHERE bb.id = aa.id AND EXISTS (SELECT 1 FROM bb b2 WHERE b2.w > aa.v) AND aa.id = 3 RETURNING aa.*;
MERGE INTO aa USING bb ON bb.id = aa.id
  WHEN MATCHED AND EXISTS (SELECT 1 FROM bb b2 WHERE b2.w = aa.v) THEN UPDATE SET v = 100
  WHEN MATCHED THEN UPDATE SET v = (SELECT max(w) FROM bb b3 WHERE b3.id = aa.id) + 1000
  WHEN NOT MATCHED THEN INSERT VALUES (bb.id, bb.w)
  RETURNING merge_action(), aa.*;
SELECT * FROM aa ORDER BY id, v;
SELECT aa.id, s.y FROM aa, LATERAL (SELECT aa.v + 1 AS y UNION ALL SELECT aa.v + 2) s ORDER BY 1, 2;
SELECT aa.id, s.y FROM aa, LATERAL (SELECT aa.v + 1 AS y UNION SELECT 7) s ORDER BY 1, 2;
SELECT aa.id, s.y FROM aa, LATERAL (VALUES (aa.v), (aa.v * 2)) s(y) ORDER BY 1, 2;
SELECT aa.id, s.y FROM aa LEFT JOIN LATERAL (SELECT aa.v + 1 AS y EXCEPT SELECT 4) s ON true ORDER BY 1, 2;
