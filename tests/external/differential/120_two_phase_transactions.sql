-- Prepared transactions retain work and locks independently of the session,
-- expose PostgreSQL's catalog contract, and resolve only by explicit GID.
SHOW max_prepared_transactions;

CREATE TABLE two_phase_rows (id integer PRIMARY KEY, value text);
INSERT INTO two_phase_rows VALUES (1, 'before');

BEGIN;
UPDATE two_phase_rows SET value = 'committed' WHERE id = 1;
INSERT INTO two_phase_rows VALUES (2, 'inserted');
PREPARE TRANSACTION 'two-phase-commit';
SELECT id, value FROM two_phase_rows ORDER BY id;
SELECT gid, owner, database, prepared IS NOT NULL, pg_typeof(transaction)
  FROM pg_prepared_xacts;
COMMIT PREPARED 'two-phase-commit';
SELECT id, value FROM two_phase_rows ORDER BY id;
SELECT count(*) FROM pg_prepared_xacts;

BEGIN;
DELETE FROM two_phase_rows WHERE id = 1;
PREPARE TRANSACTION 'two-phase-rollback';
ROLLBACK PREPARED 'two-phase-rollback';
SELECT id, value FROM two_phase_rows ORDER BY id;

BEGIN;
CREATE TABLE two_phase_created (id integer PRIMARY KEY, value text);
INSERT INTO two_phase_created VALUES (1, 'created while prepared');
PREPARE TRANSACTION 'two-phase-ddl';
SELECT count(*) FROM two_phase_created;
COMMIT PREPARED 'two-phase-ddl';
SELECT id, value FROM two_phase_created;

CREATE SEQUENCE two_phase_sequence START WITH 10;
BEGIN;
SELECT nextval('two_phase_sequence');
PREPARE TRANSACTION 'two-phase-sequence';
SELECT last_value FROM two_phase_sequence;
ROLLBACK PREPARED 'two-phase-sequence';
SELECT nextval('two_phase_sequence');

CREATE TABLE two_phase_trigger_rows (id integer PRIMARY KEY);
CREATE TABLE two_phase_trigger_audit (id integer);
CREATE FUNCTION two_phase_trigger_write() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO two_phase_trigger_audit VALUES (NEW.id); RETURN NEW; END';
CREATE CONSTRAINT TRIGGER two_phase_deferred_trigger
  AFTER INSERT ON two_phase_trigger_rows DEFERRABLE INITIALLY DEFERRED
  FOR EACH ROW EXECUTE FUNCTION two_phase_trigger_write();
BEGIN;
INSERT INTO two_phase_trigger_rows VALUES (7);
PREPARE TRANSACTION 'two-phase-trigger';
SELECT id FROM two_phase_trigger_audit;
COMMIT PREPARED 'two-phase-trigger';
SELECT id FROM two_phase_trigger_audit;

BEGIN;
INSERT INTO two_phase_rows VALUES (3, 'notification must abort');
NOTIFY two_phase_channel;
PREPARE TRANSACTION 'two-phase-notify';
SELECT count(*) FROM two_phase_rows WHERE id = 3;

COMMIT PREPARED 'missing-two-phase';
ROLLBACK PREPARED 'missing-two-phase';
BEGIN;
COMMIT PREPARED 'missing-two-phase';
ROLLBACK;

DROP TABLE two_phase_trigger_rows, two_phase_trigger_audit;
DROP FUNCTION two_phase_trigger_write();
DROP SEQUENCE two_phase_sequence;
DROP TABLE two_phase_created, two_phase_rows;
