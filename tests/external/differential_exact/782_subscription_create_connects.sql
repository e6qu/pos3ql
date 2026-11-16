-- CREATE SUBSCRIPTION connects to the publisher while it runs, whatever its
-- other options, and creates the publisher slot then; creating a slot cannot
-- be rolled back, so with create_slot it is refused inside a transaction
-- block or a function. connect = false creates the subscription offline.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/subscriptioncmds.c
CREATE SUBSCRIPTION create_sub_782 CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable' PUBLICATION create_pub_782;
BEGIN;
CREATE SUBSCRIPTION create_sub_782 CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable' PUBLICATION create_pub_782;
ROLLBACK;
BEGIN;
CREATE SUBSCRIPTION create_sub_782 CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable' PUBLICATION create_pub_782 WITH (create_slot = false);
ROLLBACK;
CREATE SUBSCRIPTION create_sub_782 CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable' PUBLICATION create_pub_782 WITH (create_slot = false, copy_data = false);
CREATE SUBSCRIPTION create_sub_782 CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable' PUBLICATION create_pub_782 WITH (enabled = false, create_slot = false, copy_data = false);
DO $$ BEGIN CREATE SUBSCRIPTION create_sub_782 CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable' PUBLICATION create_pub_782; END $$;
SELECT count(*) FROM pg_subscription;
CREATE SUBSCRIPTION create_sub_782 CONNECTION 'host=127.0.0.1 port=1 user=repl dbname=publisher sslmode=disable' PUBLICATION create_pub_782 WITH (connect = false);
SELECT subname, subenabled, subslotname FROM pg_subscription;
SELECT count(*) FROM pg_subscription_rel;
ALTER SUBSCRIPTION create_sub_782 SET (slot_name = NONE);
DROP SUBSCRIPTION create_sub_782;
