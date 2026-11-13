-- A write through an automatically updatable view into a lower view that has
-- its own INSTEAD OF trigger or rewrite rule targets that view, as
-- PostgreSQL's rewriter does: the trigger receives the composed row, its
-- returned row feeds RETURNING and the upper views' check options, and the
-- lower view's rules see the upper views' defaults.
CREATE TABLE vr_t (a int PRIMARY KEY, b int);
INSERT INTO vr_t VALUES (1, 10), (2, 20);
CREATE VIEW vr_v1 AS SELECT a, b FROM vr_t;
CREATE FUNCTION vr_trg() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% % new=% old=%', TG_OP, TG_TABLE_NAME, NEW, OLD;
  IF TG_OP = 'INSERT' THEN
    INSERT INTO vr_t VALUES (NEW.a, NEW.b * 100);
    NEW.b := NEW.b * 100;
    RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN
    UPDATE vr_t SET b = NEW.b + 1 WHERE a = OLD.a;
    NEW.b := NEW.b + 1;
    RETURN NEW;
  END IF;
  DELETE FROM vr_t WHERE a = OLD.a;
  RETURN OLD;
END $$;
CREATE TRIGGER vr_t1 INSTEAD OF INSERT OR UPDATE OR DELETE ON vr_v1
  FOR EACH ROW EXECUTE FUNCTION vr_trg();
CREATE VIEW vr_v2 AS SELECT a AS x, b * 2 AS b2, b FROM vr_v1 WHERE a < 100;
INSERT INTO vr_v2 (x, b) VALUES (3, 30) RETURNING *;
UPDATE vr_v2 SET b = 5 WHERE x = 1 RETURNING *, b2;
DELETE FROM vr_v2 WHERE x = 2 RETURNING *;
INSERT INTO vr_v2 (b2) VALUES (1);
SELECT * FROM vr_t ORDER BY a;
CREATE VIEW vr_v3 AS SELECT x, b FROM vr_v2 WHERE x > 0 WITH CHECK OPTION;
INSERT INTO vr_v3 VALUES (-1, 1);
INSERT INTO vr_v3 VALUES (200, 1);
UPDATE vr_v3 SET x = -5 WHERE x = 3;
ALTER VIEW vr_v2 ALTER COLUMN b SET DEFAULT 4;
INSERT INTO vr_v3 (x) VALUES (5) RETURNING *;
SELECT * FROM vr_t ORDER BY a;
-- Rules on the lower view.
CREATE TABLE vr_log (op text, a int, b int, c text);
CREATE TABLE vr_u (a int PRIMARY KEY, b int, c text);
INSERT INTO vr_u VALUES (1, 10, 'one'), (2, 20, 'two'), (3, 30, 'three');
CREATE VIEW vr_r1 AS SELECT a, b, c FROM vr_u;
ALTER VIEW vr_r1 ALTER COLUMN b SET DEFAULT 42;
CREATE RULE vr_r1_ins AS ON INSERT TO vr_r1 DO INSTEAD
  INSERT INTO vr_log VALUES ('ins', NEW.a, NEW.b, NEW.c) RETURNING a, b, c;
CREATE RULE vr_r1_upd AS ON UPDATE TO vr_r1 DO INSTEAD
  INSERT INTO vr_log VALUES ('upd', NEW.a, OLD.b, NEW.c);
CREATE RULE vr_r1_del AS ON DELETE TO vr_r1 DO ALSO
  INSERT INTO vr_log VALUES ('del', OLD.a, OLD.b, OLD.c);
INSERT INTO vr_r1 (a) VALUES (10);
INSERT INTO vr_r1 VALUES (11, DEFAULT);
INSERT INTO vr_r1 SELECT 12;
CREATE VIEW vr_r2 AS SELECT a AS k, c, b, b + 1 AS bp FROM vr_r1 WHERE a < 50;
ALTER VIEW vr_r2 ALTER COLUMN c SET DEFAULT 'r2';
INSERT INTO vr_r2 (k) VALUES (40);
INSERT INTO vr_r2 (k, b) VALUES (41, 4) RETURNING *;
INSERT INTO vr_r2 (k, b) VALUES (60, 5) RETURNING *;
UPDATE vr_r2 SET b = 6 WHERE k = 1;
UPDATE vr_r2 SET b = 6 WHERE k = 1 RETURNING *;
DELETE FROM vr_r2 WHERE k = 3;
DELETE FROM vr_r2 WHERE k = 2 RETURNING *;
MERGE INTO vr_r2 USING (VALUES (1)) s(k) ON vr_r2.k = s.k WHEN MATCHED THEN DELETE;
MERGE INTO vr_r1 USING (VALUES (1)) s(k) ON vr_r1.a = s.k WHEN MATCHED THEN DELETE;
CREATE VIEW vr_r3 AS SELECT k, b FROM vr_r2 WITH CHECK OPTION;
INSERT INTO vr_r3 VALUES (70, 1);
SELECT * FROM vr_log ORDER BY op, a, b;
SELECT * FROM vr_u ORDER BY a;
-- A conditional DO INSTEAD rule leaves the rest of the write to a view's
-- base, which PostgreSQL refuses unless a trigger or unconditional rule
-- handles it.
CREATE VIEW vr_c1 AS SELECT a, b FROM vr_u;
CREATE RULE vr_c1_ins AS ON INSERT TO vr_c1 WHERE NEW.b > 100 DO INSTEAD
  INSERT INTO vr_log VALUES ('big', NEW.a, NEW.b);
CREATE RULE vr_c1_upd AS ON UPDATE TO vr_c1 WHERE NEW.b > 100 DO INSTEAD NOTHING;
INSERT INTO vr_c1 VALUES (70, 1000);
UPDATE vr_c1 SET b = 1;
CREATE VIEW vr_c2 AS SELECT a, b FROM vr_c1;
INSERT INTO vr_c2 VALUES (71, 1);
SELECT count(*) FROM vr_log WHERE op = 'big';
