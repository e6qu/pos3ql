-- PostgreSQL refuses MERGE into a foreign table and transition tables on a
-- foreign table's or view's trigger; PREPARE TRANSACTION outside a block
-- warns and reports ROLLBACK; adding an existing option is a duplicate.
CREATE FOREIGN DATA WRAPPER refusal_wrapper;
CREATE SERVER refusal_server FOREIGN DATA WRAPPER refusal_wrapper OPTIONS (a '1');
CREATE FOREIGN TABLE refusal_remote (id int, note text) SERVER refusal_server;
CREATE TABLE refusal_local (id int, note text);
CREATE VIEW refusal_view AS SELECT * FROM refusal_local;
MERGE INTO refusal_remote AS t USING refusal_local AS s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET note = s.note;
CREATE FUNCTION refusal_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
CREATE TRIGGER refusal_audit AFTER INSERT ON refusal_remote
  REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION refusal_audit();
CREATE TRIGGER refusal_audit AFTER INSERT ON refusal_view
  REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION refusal_audit();
ALTER SERVER refusal_server OPTIONS (ADD a '2');
ALTER SERVER refusal_server OPTIONS (b '2', b '3');
ALTER SERVER refusal_server OPTIONS (SET a '2', SET a '3');
ALTER SERVER refusal_server OPTIONS (DROP a, DROP a);
CREATE SERVER refusal_twice FOREIGN DATA WRAPPER refusal_wrapper OPTIONS (c '1', c '2');
SELECT srvoptions FROM pg_foreign_server WHERE srvname = 'refusal_server';
PREPARE TRANSACTION 'refusal_outside';
SELECT count(*) FROM pg_prepared_xacts;
DROP FOREIGN DATA WRAPPER refusal_wrapper CASCADE;
