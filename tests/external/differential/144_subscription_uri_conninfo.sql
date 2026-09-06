-- URI and key/value conninfo are distinct PostgreSQL input spellings.  The
-- catalog retains the spelling while the bounded worker resolves it once.
DROP SUBSCRIPTION IF EXISTS uri_subscription_changes;
CREATE SUBSCRIPTION uri_subscription_changes
  CONNECTION 'postgresql://repl:secret%20word@127.0.0.1:5432/publisher?sslmode=disable&application_name=uri%20worker'
  PUBLICATION sales
  WITH (connect = false, slot_name = NONE);
SELECT subconninfo FROM pg_subscription WHERE subname = 'uri_subscription_changes';
ALTER SUBSCRIPTION uri_subscription_changes CONNECTION
  'postgres://repl@127.0.0.2:5433/publisher?sslmode=disable';
SELECT subconninfo FROM pg_subscription WHERE subname = 'uri_subscription_changes';
DROP SUBSCRIPTION uri_subscription_changes;
