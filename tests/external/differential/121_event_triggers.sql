DROP EVENT TRIGGER IF EXISTS event_trigger_a_start;
DROP EVENT TRIGGER IF EXISTS event_trigger_b_end;
DROP EVENT TRIGGER IF EXISTS event_trigger_c_drop;
DROP EVENT TRIGGER IF EXISTS event_trigger_rewrite;
DROP TABLE IF EXISTS event_trigger_target, event_trigger_audit CASCADE;
DROP FUNCTION IF EXISTS event_trigger_record() CASCADE;
DROP FUNCTION IF EXISTS event_trigger_record_rewrite() CASCADE;

CREATE TABLE event_trigger_audit
  (position serial, event text, tag text, relation_oid oid, reason integer);
CREATE TABLE event_trigger_compatible(value varchar(10));
CREATE FUNCTION event_trigger_record() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO event_trigger_audit(event, tag) VALUES (TG_EVENT, TG_TAG);
     RETURN;
   END';
CREATE FUNCTION event_trigger_record_rewrite() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN
     INSERT INTO event_trigger_audit(event, tag, relation_oid, reason)
       VALUES (TG_EVENT, TG_TAG, pg_event_trigger_table_rewrite_oid(),
               pg_event_trigger_table_rewrite_reason());
     RETURN;
   END';
CREATE EVENT TRIGGER event_trigger_b_end ON ddl_command_end
  WHEN TAG IN ('CREATE TABLE', 'DROP TABLE')
  EXECUTE FUNCTION event_trigger_record();
CREATE EVENT TRIGGER event_trigger_a_start ON ddl_command_start
  WHEN TAG IN ('CREATE TABLE', 'DROP TABLE')
  EXECUTE PROCEDURE event_trigger_record();
CREATE EVENT TRIGGER event_trigger_c_drop ON sql_drop
  WHEN TAG IN ('DROP TABLE') EXECUTE FUNCTION event_trigger_record();
CREATE EVENT TRIGGER event_trigger_rewrite ON table_rewrite
  EXECUTE FUNCTION event_trigger_record_rewrite();

CREATE TABLE event_trigger_target(value integer);
ALTER TABLE event_trigger_target ALTER COLUMN value TYPE bigint;
ALTER TABLE event_trigger_compatible ALTER COLUMN value TYPE text USING value::text;
DROP TABLE event_trigger_target;
SELECT event, tag, relation_oid IS NOT NULL, reason
FROM event_trigger_audit ORDER BY position;
SELECT evtname, evtevent, evtenabled, evttags::text
FROM pg_event_trigger ORDER BY evtname;

ALTER EVENT TRIGGER event_trigger_a_start DISABLE;
ALTER EVENT TRIGGER event_trigger_b_end RENAME TO event_trigger_b_end_renamed;
COMMENT ON EVENT TRIGGER event_trigger_b_end_renamed IS 'event trigger comment';
SELECT evtname, evtenabled, obj_description(oid, 'pg_event_trigger')
FROM pg_event_trigger WHERE evtname IN ('event_trigger_a_start', 'event_trigger_b_end_renamed')
ORDER BY evtname;

DROP EVENT TRIGGER event_trigger_a_start;
DROP EVENT TRIGGER event_trigger_b_end_renamed;
DROP EVENT TRIGGER event_trigger_c_drop;
DROP EVENT TRIGGER event_trigger_rewrite;
DROP TABLE event_trigger_audit, event_trigger_compatible;
DROP FUNCTION event_trigger_record(), event_trigger_record_rewrite();
