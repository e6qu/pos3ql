-- Declarative partitioning: definitions, routing, parent reads and writes.
CREATE TABLE partition_range (id integer PRIMARY KEY, note text) PARTITION BY RANGE (id);
CREATE TABLE partition_range_low PARTITION OF partition_range FOR VALUES FROM (MINVALUE) TO (0);
CREATE TABLE partition_range_mid PARTITION OF partition_range FOR VALUES FROM (0) TO (10);
CREATE TABLE partition_range_other PARTITION OF partition_range DEFAULT;
INSERT INTO partition_range VALUES (-1, 'low'), (1, 'one'), (9, 'nine'), (10, 'other');
SELECT id, note FROM partition_range ORDER BY id;
UPDATE partition_range SET id = 11, note = 'moved' WHERE id = 9;
DELETE FROM partition_range WHERE id = -1;
SELECT id, note FROM partition_range ORDER BY id;

CREATE TABLE partition_list (id integer PRIMARY KEY, note text) PARTITION BY LIST (id);
CREATE TABLE partition_list_one PARTITION OF partition_list FOR VALUES IN (1, 3);
CREATE TABLE partition_list_other PARTITION OF partition_list DEFAULT;
INSERT INTO partition_list VALUES (1, 'selected'), (2, 'default'), (3, 'selected');
SELECT id, note FROM partition_list ORDER BY id;

CREATE TABLE partition_hash (id integer PRIMARY KEY) PARTITION BY HASH (id);
CREATE TABLE partition_hash_zero PARTITION OF partition_hash FOR VALUES WITH (MODULUS 2, REMAINDER 0);
CREATE TABLE partition_hash_one PARTITION OF partition_hash FOR VALUES WITH (MODULUS 2, REMAINDER 1);
INSERT INTO partition_hash VALUES (1), (2), (3), (4);
SELECT id FROM partition_hash ORDER BY id;
SELECT id FROM partition_hash_zero ORDER BY id;
SELECT id FROM partition_hash_one ORDER BY id;

CREATE TABLE partition_multi (a integer, b integer, PRIMARY KEY (a, b)) PARTITION BY RANGE (a, b);
CREATE TABLE partition_multi_middle PARTITION OF partition_multi FOR VALUES FROM (0, 0) TO (10, 10);
CREATE TABLE partition_multi_other PARTITION OF partition_multi DEFAULT;
INSERT INTO partition_multi VALUES (0, 0), (10, 9), (10, 10), (-1, 9);
SELECT a, b FROM partition_multi_middle ORDER BY a, b;
SELECT a, b FROM partition_multi_other ORDER BY a, b;

CREATE TABLE partition_multi_hash (a integer, b integer) PARTITION BY HASH (a, b);
CREATE TABLE partition_multi_hash_zero PARTITION OF partition_multi_hash FOR VALUES WITH (MODULUS 4, REMAINDER 0);
CREATE TABLE partition_multi_hash_one PARTITION OF partition_multi_hash FOR VALUES WITH (MODULUS 4, REMAINDER 1);
CREATE TABLE partition_multi_hash_two PARTITION OF partition_multi_hash FOR VALUES WITH (MODULUS 4, REMAINDER 2);
CREATE TABLE partition_multi_hash_three PARTITION OF partition_multi_hash FOR VALUES WITH (MODULUS 4, REMAINDER 3);
INSERT INTO partition_multi_hash VALUES (0, 1), (1, 2), (4, 5), (8, 9), (9, 10), (11, 12);
SELECT a FROM partition_multi_hash_zero ORDER BY a;
SELECT a FROM partition_multi_hash_one ORDER BY a;
SELECT a FROM partition_multi_hash_two ORDER BY a;
SELECT a FROM partition_multi_hash_three ORDER BY a;

CREATE TABLE partition_tree (id integer, region integer) PARTITION BY RANGE (id);
CREATE TABLE partition_tree_mid PARTITION OF partition_tree FOR VALUES FROM (0) TO (100) PARTITION BY LIST (region);
CREATE TABLE partition_tree_east PARTITION OF partition_tree_mid FOR VALUES IN (1);
CREATE TABLE partition_tree_other (id integer, region integer);
ALTER TABLE partition_tree_mid ATTACH PARTITION partition_tree_other DEFAULT;
INSERT INTO partition_tree VALUES (10, 1), (20, 2);
SELECT id, region FROM partition_tree ORDER BY id;
SELECT pg_get_partkeydef(oid), pg_get_expr(relpartbound, oid)
  FROM pg_class
 WHERE relname IN ('partition_tree', 'partition_tree_mid', 'partition_tree_east', 'partition_tree_other')
 ORDER BY relname;

CREATE TABLE partition_trigger_audit (id integer, level integer);
CREATE TABLE partition_statement_audit (phase text, rows bigint);
CREATE FUNCTION partition_root_copy_audit() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO partition_trigger_audit VALUES (NEW.id, 1); RETURN NEW; END';
CREATE FUNCTION partition_leaf_copy_audit() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO partition_trigger_audit VALUES (NEW.id, 2); RETURN NEW; END';
CREATE FUNCTION partition_before_copy_audit() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO partition_statement_audit SELECT ''before'', count(*) FROM partition_tree; RETURN NULL; END';
CREATE FUNCTION partition_after_copy_audit() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO partition_statement_audit SELECT ''after'', count(*) FROM copied_rows; RETURN NULL; END';
CREATE TRIGGER partition_before_copy_audit BEFORE INSERT ON partition_tree
  FOR EACH STATEMENT EXECUTE FUNCTION partition_before_copy_audit();
CREATE TRIGGER partition_after_copy_audit AFTER INSERT ON partition_tree
  REFERENCING NEW TABLE AS copied_rows FOR EACH STATEMENT EXECUTE FUNCTION partition_after_copy_audit();
CREATE TRIGGER partition_root_copy_audit AFTER INSERT ON partition_tree
  FOR EACH ROW EXECUTE FUNCTION partition_root_copy_audit();
CREATE TRIGGER partition_leaf_copy_audit AFTER INSERT ON partition_tree_east
  FOR EACH ROW EXECUTE FUNCTION partition_leaf_copy_audit();
COPY partition_tree FROM STDIN;
30	1
\.
SELECT id, level FROM partition_trigger_audit ORDER BY id, level;
SELECT phase, rows FROM partition_statement_audit ORDER BY phase;

ALTER TABLE partition_tree_mid DETACH PARTITION partition_tree_other;
SELECT id, region FROM partition_tree ORDER BY id;
SELECT id, region FROM partition_tree_other ORDER BY id;

CREATE TABLE partition_invalid_unique (id integer UNIQUE, region integer) PARTITION BY RANGE (id);
CREATE TABLE partition_invalid_unique_mid PARTITION OF partition_invalid_unique
  FOR VALUES FROM (0) TO (10) PARTITION BY LIST (region);

CREATE TABLE partition_provenance_root (
  id integer,
  region integer,
  CONSTRAINT partition_provenance_key UNIQUE (id, region),
  CONSTRAINT partition_positive_id CHECK (id > 0)
) PARTITION BY RANGE (region);
CREATE TABLE partition_provenance_mid (
  id integer NOT NULL,
  region integer,
  CONSTRAINT partition_positive_id CHECK (id > 0)
) PARTITION BY HASH (id);
ALTER TABLE partition_provenance_root ATTACH PARTITION partition_provenance_mid
  FOR VALUES FROM (0) TO (100);
CREATE TABLE partition_provenance_leaf PARTITION OF partition_provenance_mid
  FOR VALUES WITH (MODULUS 1, REMAINDER 0);
ALTER TABLE partition_provenance_root ALTER COLUMN id SET NOT NULL;
ALTER TABLE partition_provenance_root ALTER COLUMN id DROP NOT NULL;
SELECT relation.relname, constraint_catalog.conislocal, constraint_catalog.coninhcount
  FROM pg_constraint constraint_catalog
  JOIN pg_class relation ON relation.oid = constraint_catalog.conrelid
 WHERE relation.relname IN ('partition_provenance_mid', 'partition_provenance_leaf')
   AND constraint_catalog.contype = 'n'
 ORDER BY relation.relname;
ALTER TABLE partition_provenance_root RENAME CONSTRAINT partition_positive_id TO partition_id_positive;
ALTER TABLE partition_provenance_root DROP CONSTRAINT partition_provenance_key;
SELECT relation.relname, constraint_catalog.conname
  FROM pg_constraint constraint_catalog
  JOIN pg_class relation ON relation.oid = constraint_catalog.conrelid
 WHERE relation.relname LIKE 'partition_provenance_%'
   AND constraint_catalog.contype IN ('c', 'u')
 ORDER BY relation.relname, constraint_catalog.conname;
