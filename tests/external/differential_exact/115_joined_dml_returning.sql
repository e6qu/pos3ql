-- RETURNING of UPDATE ... FROM and DELETE ... USING sees every FROM item:
-- `*` lists the target and then each item, `s.*` and `s` name one item, and an
-- unqualified name present in two of them is ambiguous.
CREATE TABLE t115(a int, b text); CREATE TABLE s115(a int, c text, d numeric(5,2));
INSERT INTO t115 VALUES (1,'x'),(2,'y'); INSERT INTO s115 VALUES (1,'p',10),(2,'q',20);
UPDATE t115 SET b = s115.c FROM s115 WHERE t115.a = s115.a RETURNING *;
UPDATE t115 SET b = s.c FROM s115 s WHERE t115.a = s.a RETURNING s.*, t115.b, s, old.b, new.b, s.d + 1;
UPDATE t115 SET b = s.c FROM s115 s WHERE t115.a = s.a RETURNING (SELECT s.d * 2), (s).c, (s.*).d;
UPDATE t115 SET b = c FROM s115 WHERE t115.a = s115.a RETURNING a;
UPDATE t115 SET b = 'z' FROM s115 WHERE t115.a = s115.a RETURNING nope;
UPDATE t115 SET b = s.c FROM s115 s, (VALUES (1)) v(z) WHERE t115.a = s.a AND v.z = s.a RETURNING *;
UPDATE t115 SET b = s.c FROM s115 s JOIN (VALUES (2,'j')) v(z,w) ON v.z = s.a WHERE t115.a = s.a RETURNING *;
UPDATE t115 SET b = v.y FROM (VALUES (1, 'v1')) v(x, y) LEFT JOIN s115 ON s115.a = 9 WHERE t115.a = v.x RETURNING *;
UPDATE t115 SET b = 'u' FROM s115 JOIN s115 s2 USING (a) WHERE t115.a = s115.a RETURNING *, s2.*;
UPDATE t115 SET b = 'n' FROM s115 NATURAL JOIN s115 s2 WHERE t115.a = s115.a RETURNING *;
UPDATE t115 SET b = 'k' FROM s115 JOIN s115 s2 USING (a) AS j WHERE t115.a = s115.a RETURNING j.*, j.a;
UPDATE t115 SET b = 'g' FROM generate_series(1,1) g WHERE t115.a = g RETURNING *;
WITH u AS (UPDATE t115 SET b = c FROM s115 WHERE t115.a = s115.a RETURNING *) SELECT * FROM u ORDER BY 1;
DELETE FROM t115 USING s115 WHERE t115.a = s115.a AND s115.d = 10 RETURNING *, s115.c;
DELETE FROM t115 t USING t115 u WHERE t.a = u.a RETURNING *;
INSERT INTO t115 VALUES (1,'x'),(2,'y');
MERGE INTO t115 USING s115 s ON t115.a = s.a WHEN MATCHED THEN UPDATE SET b = 'm' RETURNING *;
-- Views pass the FROM items through, with or without an INSTEAD OF trigger.
CREATE VIEW v115 AS SELECT a, b FROM t115;
UPDATE v115 SET b = c FROM s115 WHERE v115.a = s115.a RETURNING *;
DELETE FROM v115 USING s115 WHERE v115.a = s115.a AND s115.a = 1 RETURNING *, s115.d;
CREATE TABLE tv115(a int, b text); INSERT INTO tv115 VALUES (2, 'k');
CREATE VIEW vt115 AS SELECT * FROM tv115;
CREATE FUNCTION vt115_trig() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; RETURN NEW; END $$;
CREATE TRIGGER vt115_t INSTEAD OF UPDATE OR DELETE ON vt115 FOR EACH ROW EXECUTE FUNCTION vt115_trig();
UPDATE vt115 SET b = s115.c FROM s115 WHERE vt115.a = s115.a RETURNING *;
DELETE FROM vt115 USING s115 WHERE vt115.a = s115.a RETURNING *;
-- A comma starts a new FROM item: its USING and NATURAL joins merge only
-- that item's columns.
SELECT * FROM t115, s115 JOIN s115 s2 USING (a) ORDER BY 1, 3;
SELECT * FROM t115, s115 NATURAL JOIN t115 t2 ORDER BY 1, 3;
SELECT * FROM s115 JOIN s115 s2 USING (a), t115 ORDER BY 1, 6;
DROP VIEW v115, vt115;
DROP FUNCTION vt115_trig() CASCADE;
DROP TABLE t115, s115, tv115;
