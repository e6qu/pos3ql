-- DROP SUBSCRIPTION drops the publisher slot before it commits. A slot drop
-- cannot be rolled back, so it is refused inside a transaction block, and an
-- unreachable publisher fails the drop and keeps the subscription.
CREATE SUBSCRIPTION slot_drop_sub
  CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable'
  PUBLICATION slot_drop_pub WITH (connect = false);
SELECT subname, subenabled, subslotname FROM pg_subscription;
DROP SUBSCRIPTION slot_drop_sub, other_sub;
BEGIN;
DROP SUBSCRIPTION slot_drop_sub;
ROLLBACK;
DROP SUBSCRIPTION slot_drop_sub;
DROP SUBSCRIPTION IF EXISTS slot_drop_sub CASCADE;
ALTER SUBSCRIPTION slot_drop_sub SET (slot_name = 'renamed_slot');
DROP SUBSCRIPTION slot_drop_sub RESTRICT;
SELECT subname, subslotname FROM pg_subscription;
-- Without a slot the drop is local and transactional.
ALTER SUBSCRIPTION slot_drop_sub SET (slot_name = NONE);
BEGIN;
DROP SUBSCRIPTION slot_drop_sub;
SELECT count(*) FROM pg_subscription;
ROLLBACK;
SELECT subname, subslotname FROM pg_subscription;
DROP SUBSCRIPTION slot_drop_sub CASCADE;
DROP SUBSCRIPTION IF EXISTS slot_drop_sub;
SELECT count(*) FROM pg_subscription;
