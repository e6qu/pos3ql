CREATE TABLE ordered_partition_trigger_log (position serial, name text);
CREATE TABLE ordered_partition_trigger_root (id integer) PARTITION BY RANGE (id);
CREATE TABLE ordered_partition_trigger_leaf PARTITION OF ordered_partition_trigger_root
  FOR VALUES FROM (0) TO (10);
CREATE FUNCTION ordered_partition_trigger() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO ordered_partition_trigger_log (name) VALUES (TG_NAME || '':'' || TG_LEVEL); RETURN NEW; END';
CREATE TRIGGER z_before BEFORE INSERT ON ordered_partition_trigger_root
  FOR EACH ROW EXECUTE FUNCTION ordered_partition_trigger();
CREATE TRIGGER a_before BEFORE INSERT ON ordered_partition_trigger_leaf
  FOR EACH ROW EXECUTE FUNCTION ordered_partition_trigger();
CREATE TRIGGER z_after AFTER INSERT ON ordered_partition_trigger_root
  FOR EACH ROW EXECUTE FUNCTION ordered_partition_trigger();
CREATE TRIGGER a_after AFTER INSERT ON ordered_partition_trigger_leaf
  FOR EACH ROW EXECUTE FUNCTION ordered_partition_trigger();
CREATE TRIGGER a_before BEFORE INSERT ON ordered_partition_trigger_root
  FOR EACH STATEMENT EXECUTE FUNCTION ordered_partition_trigger();
CREATE TRIGGER a_after AFTER INSERT ON ordered_partition_trigger_root
  FOR EACH STATEMENT EXECUTE FUNCTION ordered_partition_trigger();
INSERT INTO ordered_partition_trigger_root VALUES (1);
SELECT name FROM ordered_partition_trigger_log ORDER BY position;
DROP TABLE ordered_partition_trigger_root;
DROP TABLE ordered_partition_trigger_log;
DROP FUNCTION ordered_partition_trigger();
