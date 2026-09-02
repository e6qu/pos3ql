-- A disabled subscription is a durable local catalog object.  `connect =
-- false` is PostgreSQL's explicit declaration that no publisher contact, slot
-- creation, or initial copy happens at CREATE time. `slot_name = NONE` makes
-- the later drop local too.
DROP SUBSCRIPTION IF EXISTS archived_changes;
CREATE SUBSCRIPTION archived_changes
  CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable'
  PUBLICATION sales, inventory
  WITH (connect = false, slot_name = NONE, binary = true, streaming = parallel,
        synchronous_commit = remote_apply, two_phase = false,
        disable_on_error = true, password_required = false,
        run_as_owner = true, origin = none, failover = true);
COMMENT ON SUBSCRIPTION archived_changes IS 'archived stream';
SELECT obj_description(oid, 'pg_subscription') FROM pg_subscription
 WHERE subname = 'archived_changes';
SELECT subname, subenabled, subconninfo, subslotname, subpublications,
       subbinary, substream, subsynccommit, subdisableonerr,
       subpasswordrequired, subrunasowner, suborigin, subfailover
  FROM pg_subscription
  WHERE subname = 'archived_changes';
ALTER SUBSCRIPTION archived_changes
  CONNECTION 'host=127.0.0.2 port=5433 user=repl dbname=publisher sslmode=disable';
ALTER SUBSCRIPTION archived_changes SET PUBLICATION inventory, sales WITH (refresh = false);
BEGIN;
ALTER SUBSCRIPTION archived_changes SET (slot_name = archived_changes_slot);
ALTER SUBSCRIPTION archived_changes ENABLE;
SELECT subenabled, subslotname FROM pg_subscription WHERE subname = 'archived_changes';
ROLLBACK;
ALTER SUBSCRIPTION archived_changes SET
  (binary = false, streaming = on, synchronous_commit = local,
   disable_on_error = false, password_required = true,
   run_as_owner = false, origin = any, failover = false, two_phase = false);
ALTER SUBSCRIPTION archived_changes SKIP (lsn = '0/2A');
ALTER SUBSCRIPTION archived_changes RENAME TO archived_changes_renamed;
SELECT subname, subenabled, subconninfo, subslotname, subpublications,
       subskiplsn, subbinary, substream, subsynccommit, subdisableonerr,
       subpasswordrequired, subrunasowner, suborigin, subfailover
  FROM pg_subscription
  WHERE subname = 'archived_changes_renamed';
SELECT obj_description(oid, 'pg_subscription') FROM pg_subscription
 WHERE subname = 'archived_changes_renamed';
ALTER SUBSCRIPTION archived_changes_renamed SKIP (lsn = NONE);
DROP SUBSCRIPTION archived_changes_renamed;
SELECT count(*) FROM pg_subscription WHERE subname = 'archived_changes_renamed';
