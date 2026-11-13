-- MERGE into a view whose every writing action has an INSTEAD OF row
-- trigger fires those triggers, directly or through automatically updatable
-- views above it: statement triggers, row triggers with OLD and NEW,
-- RETURNING with merge_action(), and the errors for missing triggers.
CREATE SCHEMA merge_view_triggers;
SET search_path = merge_view_triggers, public;
CREATE TABLE base (id int PRIMARY KEY, label text);
INSERT INTO base VALUES (1, 'one'), (2, 'two'), (3, 'three');
CREATE VIEW v AS SELECT id, label, id * 10 AS tens FROM base;
CREATE FUNCTION vt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_LEVEL = 'STATEMENT' THEN
    RAISE NOTICE '% STATEMENT % on %', TG_WHEN, TG_OP, TG_TABLE_NAME;
    RETURN NULL;
  END IF;
  IF TG_OP = 'INSERT' THEN
    RAISE NOTICE 'INSTEAD OF INSERT new=%', NEW;
    IF NEW.label = 'skip' THEN RETURN NULL; END IF;
    INSERT INTO base VALUES (NEW.id, NEW.label);
    NEW.tens := NEW.id * 10;
    RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN
    RAISE NOTICE 'INSTEAD OF UPDATE old=% new=%', OLD, NEW;
    IF NEW.label = 'skip' THEN RETURN NULL; END IF;
    UPDATE base SET id = NEW.id, label = NEW.label WHERE id = OLD.id;
    RETURN NEW;
  ELSE
    RAISE NOTICE 'INSTEAD OF DELETE old=%', OLD;
    DELETE FROM base WHERE id = OLD.id;
    RETURN OLD;
  END IF;
END $$;
CREATE TRIGGER v_row INSTEAD OF INSERT OR UPDATE OR DELETE ON v
  FOR EACH ROW EXECUTE FUNCTION vt();
CREATE TRIGGER v_before BEFORE INSERT OR UPDATE OR DELETE ON v
  FOR EACH STATEMENT EXECUTE FUNCTION vt();
CREATE TRIGGER v_after AFTER INSERT OR UPDATE OR DELETE ON v
  FOR EACH STATEMENT EXECUTE FUNCTION vt();
CREATE TABLE src (id int, label text);
INSERT INTO src VALUES (1, 'uno'), (4, 'cuatro');
MERGE INTO v USING src s ON v.id = s.id
  WHEN MATCHED THEN UPDATE SET label = s.label
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.label)
  RETURNING merge_action(), v.*, s.*;
SELECT * FROM base ORDER BY id;
MERGE INTO v AS t USING (VALUES (2)) AS s(k) ON t.id = s.k
  WHEN MATCHED THEN DELETE
  WHEN NOT MATCHED BY SOURCE AND t.id = 3 THEN UPDATE SET label = 'by source'
  RETURNING merge_action(), t.*, s.k, old, new;
SELECT * FROM base ORDER BY id;
-- A view row can be acted on twice; a trigger returning NULL skips its row.
MERGE INTO v USING (VALUES (1, 'dup'), (1, 'skip'), (8, 'skip'), (9, 'nine')) AS s(k, l)
  ON v.id = s.k
  WHEN MATCHED THEN UPDATE SET label = s.l
  WHEN NOT MATCHED THEN INSERT (id, label) VALUES (s.k, s.l)
  RETURNING merge_action(), v, s;
MERGE INTO v USING src s ON v.id = s.id WHEN MATCHED THEN UPDATE SET tens = 5;
MERGE INTO v USING src s ON v.id = s.id
  WHEN MATCHED AND s.id = 4 THEN DO NOTHING
  WHEN MATCHED THEN UPDATE SET label = DEFAULT
  RETURNING old.label, new.label;
SELECT * FROM base ORDER BY id;
-- Through an automatically updatable view: its filter selects the rows and
-- its check option applies to the rows the triggers return.
CREATE VIEW v2 AS SELECT id, label FROM v WHERE id < 5 WITH CHECK OPTION;
MERGE INTO v2 USING (VALUES (1, 'w'), (3, 'th'), (9, 'x')) AS s(k, l) ON v2.id = s.k
  WHEN MATCHED THEN UPDATE SET label = s.l
  RETURNING merge_action(), v2.*;
MERGE INTO v2 USING (VALUES (7, 'seven')) AS s(k, l) ON v2.id = s.k
  WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.l);
MERGE INTO v2 USING (VALUES (1)) AS s(k) ON v2.id = s.k
  WHEN NOT MATCHED BY SOURCE AND v2.id > 3 THEN DELETE
  RETURNING merge_action(), v2.*;
SELECT * FROM base ORDER BY id;
-- Views without a full set of triggers.
CREATE VIEW dv AS SELECT DISTINCT id, label FROM base;
CREATE FUNCTION nt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'nt % %', TG_OP, NEW; RETURN NULL; END $$;
CREATE TRIGGER dv_update INSTEAD OF UPDATE ON dv FOR EACH ROW EXECUTE FUNCTION nt();
MERGE INTO dv USING src s ON dv.id = s.id
  WHEN MATCHED THEN UPDATE SET label = 'x'
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.label);
MERGE INTO dv USING src s ON dv.id = s.id
  WHEN MATCHED THEN UPDATE SET label = 'x' RETURNING merge_action(), dv.*;
MERGE INTO dv USING src s ON dv.id = s.id WHEN MATCHED THEN DO NOTHING;
CREATE VIEW pv AS SELECT id, label FROM base;
CREATE TRIGGER pv_update INSTEAD OF UPDATE ON pv FOR EACH ROW EXECUTE FUNCTION nt();
MERGE INTO pv USING src s ON pv.id = s.id
  WHEN MATCHED THEN UPDATE SET label = 'x'
  WHEN NOT MATCHED THEN INSERT VALUES (s.id + 100, s.label);
MERGE INTO pv USING src s ON pv.id = s.id
  WHEN MATCHED THEN UPDATE SET label = 'x';
CREATE VIEW pv2 AS SELECT * FROM pv;
MERGE INTO pv2 USING src s ON pv2.id = s.id
  WHEN MATCHED THEN UPDATE SET label = 'y'
  WHEN NOT MATCHED THEN INSERT VALUES (s.id + 100, s.label);
MERGE INTO pv2 USING src s ON pv2.id = s.id
  WHEN NOT MATCHED THEN INSERT VALUES (s.id + 200, s.label);
SELECT * FROM base ORDER BY id;
CREATE VIEW iv AS SELECT id, label FROM base;
CREATE TRIGGER iv_insert INSTEAD OF INSERT ON iv FOR EACH ROW EXECUTE FUNCTION nt();
CREATE VIEW iv2 AS SELECT * FROM iv WHERE id > 0;
MERGE INTO iv2 USING src s ON iv2.id = s.id
  WHEN NOT MATCHED THEN INSERT VALUES (s.id + 300, s.label);
MERGE INTO v USING src s ON v.id = s.id WHEN MATCHED THEN UPDATE SET nosuch = 1;
MERGE INTO v USING src s ON v.id = s.id WHEN NOT MATCHED THEN INSERT (id, id) VALUES (1, 2);
-- Inside PL/pgSQL.
DO $$
DECLARE n int;
BEGIN
  MERGE INTO v USING src s ON v.id = s.id WHEN MATCHED THEN UPDATE SET label = s.label || '!';
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'merged %', n;
END $$;
SELECT * FROM base ORDER BY id;
DROP SCHEMA merge_view_triggers CASCADE;
-- Trigger bodies that write a view run on the engine between attempts.
CREATE SCHEMA merge_view_engine;
SET search_path = merge_view_engine, public;
CREATE TABLE base (id int PRIMARY KEY, label text);
INSERT INTO base VALUES (1, 'one'), (2, 'two'), (3, 'three');
CREATE TABLE log (msg text);
CREATE VIEW logv AS SELECT msg FROM log;
CREATE VIEW v AS SELECT id, label FROM base;
CREATE FUNCTION vt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO logv VALUES (TG_OP || ' ' || coalesce(OLD.id::text, '-') || ' ' || coalesce(NEW.id::text, '-'));
  IF TG_OP = 'INSERT' THEN INSERT INTO base VALUES (NEW.id, NEW.label); RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN UPDATE base SET label = NEW.label WHERE id = OLD.id; RETURN NEW;
  ELSE DELETE FROM base WHERE id = OLD.id; RETURN OLD; END IF;
END $$;
CREATE TRIGGER v_row INSTEAD OF INSERT OR UPDATE OR DELETE ON v FOR EACH ROW EXECUTE FUNCTION vt();
MERGE INTO v USING (VALUES (1, 'a'), (2, NULL), (5, 'e')) AS s(k, l) ON v.id = s.k
  WHEN MATCHED AND s.l IS NULL THEN DELETE
  WHEN MATCHED THEN UPDATE SET label = s.l
  WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.l)
  RETURNING merge_action(), v.*, (SELECT count(*) FROM log) AS logged;
SELECT * FROM base ORDER BY id;
SELECT * FROM log ORDER BY msg;
BEGIN;
MERGE INTO v USING (VALUES (1, 'b')) AS s(k, l) ON v.id = s.k
  WHEN MATCHED THEN UPDATE SET label = s.l;
ROLLBACK;
SELECT * FROM base ORDER BY id;
DROP SCHEMA merge_view_engine CASCADE;
