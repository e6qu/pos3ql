DROP TABLE IF EXISTS rewrite_rule_mode_source;
DROP TABLE IF EXISTS rewrite_rule_mode_audit;

CREATE TABLE rewrite_rule_mode_source (id integer);
CREATE TABLE rewrite_rule_mode_audit (id integer);
CREATE RULE rewrite_rule_mode_audit AS ON INSERT TO rewrite_rule_mode_source
  DO ALSO INSERT INTO rewrite_rule_mode_audit VALUES (NEW.id);

ALTER TABLE rewrite_rule_mode_source DISABLE RULE rewrite_rule_mode_audit;
INSERT INTO rewrite_rule_mode_source VALUES (1);
SELECT ev_enabled FROM pg_rewrite WHERE rulename = 'rewrite_rule_mode_audit';
SELECT id FROM rewrite_rule_mode_audit ORDER BY id;

ALTER TABLE rewrite_rule_mode_source ENABLE REPLICA RULE rewrite_rule_mode_audit;
INSERT INTO rewrite_rule_mode_source VALUES (2);
SELECT ev_enabled FROM pg_rewrite WHERE rulename = 'rewrite_rule_mode_audit';
SELECT id FROM rewrite_rule_mode_audit ORDER BY id;

ALTER TABLE rewrite_rule_mode_source ENABLE ALWAYS RULE rewrite_rule_mode_audit;
INSERT INTO rewrite_rule_mode_source VALUES (3);
SELECT ev_enabled FROM pg_rewrite WHERE rulename = 'rewrite_rule_mode_audit';
SELECT id FROM rewrite_rule_mode_audit ORDER BY id;

ALTER TABLE rewrite_rule_mode_source ENABLE RULE rewrite_rule_mode_audit;
INSERT INTO rewrite_rule_mode_source VALUES (4);
SELECT ev_enabled FROM pg_rewrite WHERE rulename = 'rewrite_rule_mode_audit';
SELECT id FROM rewrite_rule_mode_audit ORDER BY id;

DROP TABLE rewrite_rule_mode_source;
DROP TABLE rewrite_rule_mode_audit;
