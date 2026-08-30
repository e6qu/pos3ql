DROP TABLE IF EXISTS rewrite_source, rewrite_log CASCADE;

CREATE TABLE rewrite_source(id integer DEFAULT 40, value integer DEFAULT 1);
CREATE TABLE rewrite_log(kind text, id integer, old_value integer, new_value integer);

CREATE RULE a_insert AS ON INSERT TO rewrite_source DO ALSO
  INSERT INTO rewrite_log VALUES ('insert', NEW.id, NULL, NEW.value);
CREATE RULE z_update AS ON UPDATE TO rewrite_source
  WHERE NEW.value >= 5 DO ALSO
  INSERT INTO rewrite_log VALUES ('update', OLD.id, OLD.value, NEW.value);
CREATE RULE keep_delete AS ON DELETE TO rewrite_source
  WHERE OLD.id = 40 DO INSTEAD
  INSERT INTO rewrite_log VALUES ('kept', OLD.id, OLD.value, NULL);

INSERT INTO rewrite_source(value) VALUES (DEFAULT), (5);
UPDATE rewrite_source SET value = value + 4;
DELETE FROM rewrite_source;
SELECT id, value FROM rewrite_source ORDER BY id, value;
SELECT kind, id, old_value, new_value FROM rewrite_log ORDER BY kind, id, new_value;

COMMENT ON RULE a_insert ON rewrite_source IS 'rewrite differential';
ALTER RULE a_insert ON rewrite_source RENAME TO b_insert;
SELECT r.rulename, r.ev_type, r.is_instead,
       obj_description(r.oid, 'pg_rewrite'),
       pg_get_ruledef(r.oid) LIKE '%b_insert%'
FROM pg_rewrite r
WHERE r.rulename IN ('b_insert', 'keep_delete', 'z_update')
ORDER BY r.rulename;
SELECT schemaname, tablename, rulename
FROM pg_rules WHERE tablename = 'rewrite_source' ORDER BY rulename;
SELECT relhasrules FROM pg_class WHERE relname = 'rewrite_source';

BEGIN;
DROP RULE b_insert ON rewrite_source;
ROLLBACK;
SELECT count(*) FROM pg_rules WHERE rulename = 'b_insert';

CREATE OR REPLACE RULE b_insert AS ON INSERT TO rewrite_source DO INSTEAD NOTHING;
SELECT is_instead, pg_get_ruledef(oid) LIKE '%INSTEAD NOTHING%'
FROM pg_rewrite WHERE rulename = 'b_insert';
DROP RULE b_insert ON rewrite_source;
DROP RULE keep_delete ON rewrite_source;
DROP RULE z_update ON rewrite_source;
SELECT relhasrules FROM pg_class WHERE relname = 'rewrite_source';

DROP TABLE rewrite_source, rewrite_log;
