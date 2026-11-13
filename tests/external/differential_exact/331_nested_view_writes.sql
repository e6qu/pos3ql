-- Writes through views on views and views with computed columns rewrite to
-- the base table level by level, as PostgreSQL's rewriteTargetView does: a
-- written computed column fails at the level that computes it, view filters
-- and check options compose, and MERGE writes through the same rewrite.
CREATE TABLE nvw_t (a int PRIMARY KEY, b int, c text DEFAULT 'cd');
INSERT INTO nvw_t VALUES (1, 10, 'x'), (2, 20, 'y'), (3, -5, 'z');
CREATE VIEW nvw_alias AS SELECT a, b FROM nvw_t x WHERE x.b > 0;
UPDATE nvw_alias SET b = b + 1 WHERE a = 1 RETURNING *;
CREATE VIEW nvw_v1 AS SELECT a, b * 2 AS b2, c, b FROM nvw_t WHERE a > 0;
CREATE VIEW nvw_v2 AS SELECT a AS aa, b2, c FROM nvw_v1 WHERE b2 > 5;
SELECT * FROM nvw_v2 ORDER BY 1;
INSERT INTO nvw_v1 (a, c) VALUES (4, 'w') RETURNING *;
INSERT INTO nvw_v1 (a, b2) VALUES (5, 3);
UPDATE nvw_v1 SET b2 = 3;
UPDATE nvw_v2 SET c = 'q' WHERE aa = 2 RETURNING *, b2 + 1;
UPDATE nvw_v2 SET c = 'r' WHERE b2 = 40 RETURNING nvw_v2.*;
UPDATE nvw_v2 SET c = 's' WHERE aa = 2 RETURNING nvw_v2;
DELETE FROM nvw_v2 WHERE b2 = 22 RETURNING *;
INSERT INTO nvw_v2 (aa, c) VALUES (6, 'six') RETURNING *;
UPDATE nvw_v2 SET b2 = 1;
INSERT INTO nvw_v2 VALUES (7, 1, 'e');
SELECT * FROM nvw_t ORDER BY a;
-- A view default on a computed column makes an INSERT write it.
ALTER VIEW nvw_v1 ALTER COLUMN b2 SET DEFAULT 7;
INSERT INTO nvw_v1 (a) VALUES (8);
INSERT INTO nvw_v1 VALUES (8);
ALTER VIEW nvw_v1 ALTER COLUMN b2 DROP DEFAULT;
ALTER VIEW nvw_v1 ALTER COLUMN c SET DEFAULT 'vdef';
INSERT INTO nvw_v2 (aa) VALUES (11) RETURNING *;
ALTER VIEW nvw_v2 ALTER COLUMN c SET DEFAULT 'v2def';
INSERT INTO nvw_v2 (aa) VALUES (12) RETURNING *;
CREATE VIEW nvw_first AS SELECT b * 2 AS b2, a, c FROM nvw_t;
INSERT INTO nvw_first VALUES (DEFAULT, 13);
INSERT INTO nvw_first (a) VALUES (13) RETURNING *;
CREATE VIEW nvw_dup AS SELECT a, a AS a2, c FROM nvw_t;
INSERT INTO nvw_dup VALUES (14, 15);
UPDATE nvw_dup SET a = 1, a2 = 2;
UPDATE nvw_dup SET a2 = 16 WHERE a = 13 RETURNING *;
CREATE VIEW nvw_whole AS SELECT a, nvw_t AS whole, c FROM nvw_t;
INSERT INTO nvw_whole (a, whole) VALUES (1, NULL);
UPDATE nvw_whole SET c = 'k' WHERE a = 16 RETURNING *;
CREATE VIEW nvw_none AS SELECT a + 1 AS x FROM nvw_t;
DELETE FROM nvw_none WHERE x = 9 RETURNING *;
INSERT INTO nvw_none VALUES (1);
UPDATE nvw_none SET x = 1;
SELECT table_name, is_updatable, is_insertable_into FROM information_schema.views WHERE table_name LIKE 'nvw%' ORDER BY 1;
SELECT table_name, column_name, is_updatable FROM information_schema.columns WHERE table_name LIKE 'nvw%' AND table_name <> 'nvw_t' ORDER BY 1, ordinal_position;
-- Check options: the innermost failing view is reported, and CASCADED
-- extends to the views below.
CREATE VIEW nvw_c3 AS SELECT a, b FROM nvw_t WHERE b > 0 WITH CHECK OPTION;
CREATE VIEW nvw_c4 AS SELECT a, b FROM nvw_c3 WHERE a < 100 WITH LOCAL CHECK OPTION;
INSERT INTO nvw_c4 VALUES (200, 1);
INSERT INTO nvw_c4 VALUES (30, -1);
CREATE VIEW nvw_c5 AS SELECT a, b FROM nvw_c3 WHERE a < 100;
INSERT INTO nvw_c5 VALUES (31, -1);
CREATE VIEW nvw_c6 AS SELECT a, b FROM nvw_alias WHERE a < 100 WITH CASCADED CHECK OPTION;
INSERT INTO nvw_c6 VALUES (32, -1);
INSERT INTO nvw_c6 VALUES (101, 1);
UPDATE nvw_c4 SET b = -b WHERE a = 2;
-- MERGE into automatically updatable views.
MERGE INTO nvw_v1 USING (VALUES (1, 'm1'), (50, 'm50')) s(k, txt) ON nvw_v1.a = s.k WHEN MATCHED THEN UPDATE SET c = s.txt WHEN NOT MATCHED THEN INSERT (a, c) VALUES (s.k, s.txt) RETURNING merge_action(), nvw_v1.*;
MERGE INTO nvw_v1 USING (VALUES (2)) s(k) ON nvw_v1.a = s.k WHEN MATCHED THEN UPDATE SET b2 = 1;
MERGE INTO nvw_v2 USING (VALUES (2)) s(k) ON nvw_v2.aa = s.k WHEN MATCHED THEN DELETE RETURNING *;
MERGE INTO nvw_alias v USING (VALUES (3, 9)) s(k, nb) ON v.a = s.k WHEN MATCHED THEN UPDATE SET b = s.nb WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.nb) RETURNING merge_action(), v.*, v;
MERGE INTO nvw_c3 USING (VALUES (60, -1)) s(k, nb) ON nvw_c3.a = s.k WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.nb);
CREATE VIEW nvw_computed AS SELECT a, b, b + 1 AS bp FROM nvw_t;
MERGE INTO nvw_computed USING (VALUES (3)) s(k) ON nvw_computed.a = s.k WHEN NOT MATCHED THEN INSERT VALUES (3, 1, 2);
SELECT * FROM nvw_t ORDER BY a;
-- Each view below the written one is checked as the owner of the view
-- above it.
CREATE ROLE nvw_alice;
CREATE ROLE nvw_bob;
CREATE TABLE nvw_p (a int PRIMARY KEY, b int, c text);
INSERT INTO nvw_p VALUES (1, 1, 'x');
CREATE VIEW nvw_p1 AS SELECT a, b, c FROM nvw_p WHERE b > 0;
CREATE VIEW nvw_p2 AS SELECT a, b, c FROM nvw_p1;
ALTER VIEW nvw_p1 OWNER TO nvw_alice;
ALTER VIEW nvw_p2 OWNER TO nvw_bob;
GRANT SELECT, INSERT, UPDATE, DELETE ON nvw_p2 TO PUBLIC;
SET ROLE nvw_bob;
INSERT INTO nvw_p2 VALUES (3, 3, 'z');
RESET ROLE;
GRANT INSERT, UPDATE ON nvw_p1 TO nvw_bob;
SET ROLE nvw_bob;
INSERT INTO nvw_p2 VALUES (3, 3, 'z');
RESET ROLE;
GRANT ALL ON nvw_p TO nvw_alice;
SET ROLE nvw_bob;
INSERT INTO nvw_p2 VALUES (3, 3, 'z') RETURNING *;
UPDATE nvw_p2 SET c = 'q' RETURNING *;
DELETE FROM nvw_p2 WHERE a = 3;
MERGE INTO nvw_p2 USING (VALUES (1)) s(k) ON nvw_p2.a = s.k WHEN MATCHED THEN UPDATE SET c = 'm' RETURNING *;
RESET ROLE;
GRANT DELETE ON nvw_p1 TO nvw_bob;
SET ROLE nvw_bob;
DELETE FROM nvw_p2 WHERE a = 3 RETURNING *;
RESET ROLE;
CREATE TABLE nvw_o (a int, c text);
INSERT INTO nvw_o VALUES (1, 'o1');
UPDATE nvw_p2 SET c = nvw_o.c FROM nvw_o WHERE nvw_o.a = nvw_p2.a RETURNING *;
DELETE FROM nvw_p2 USING nvw_o WHERE nvw_o.a = nvw_p2.a RETURNING *;
INSERT INTO nvw_p1 VALUES (10, 5, 'p') ON CONFLICT (a) DO UPDATE SET c = 'conf' RETURNING *;
INSERT INTO nvw_p1 VALUES (10, 5, 'p') ON CONFLICT (a) DO UPDATE SET c = excluded.c || 'x' RETURNING *;
DROP VIEW nvw_p2, nvw_p1;
DROP TABLE nvw_p, nvw_o;
DROP ROLE nvw_alice;
DROP ROLE nvw_bob;
