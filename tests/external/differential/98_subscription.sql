-- A disabled subscription is a durable local catalog object.  `connect =
-- false` is PostgreSQL's explicit declaration that no publisher contact, slot
-- creation, or initial copy happens at CREATE time. `slot_name = NONE` makes
-- the later drop local too.
DROP SUBSCRIPTION IF EXISTS archived_changes;
CREATE SUBSCRIPTION archived_changes
  CONNECTION 'host=publisher port=5432'
  PUBLICATION sales, inventory
  WITH (connect = false, slot_name = NONE);
SELECT subname, subenabled, subconninfo, subslotname, subpublications
  FROM pg_subscription
  WHERE subname = 'archived_changes';
ALTER SUBSCRIPTION archived_changes
  CONNECTION 'host=publisher-two port=5433';
ALTER SUBSCRIPTION archived_changes SET PUBLICATION inventory, sales WITH (refresh = false);
SELECT subname, subenabled, subconninfo, subslotname, subpublications
  FROM pg_subscription
  WHERE subname = 'archived_changes';
DROP SUBSCRIPTION archived_changes;
SELECT count(*) FROM pg_subscription WHERE subname = 'archived_changes';
