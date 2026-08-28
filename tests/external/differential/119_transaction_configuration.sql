RESET ALL;

SHOW default_transaction_isolation;
SHOW default_transaction_read_only;
SHOW default_transaction_deferrable;
SELECT name, setting, reset_val, source
FROM pg_settings
WHERE name IN ('default_transaction_isolation',
               'default_transaction_read_only',
               'default_transaction_deferrable',
               'transaction_isolation',
               'transaction_read_only',
               'transaction_deferrable')
ORDER BY name;

SET SESSION CHARACTERISTICS AS TRANSACTION
  ISOLATION LEVEL SERIALIZABLE, READ ONLY, DEFERRABLE;
SHOW default_transaction_isolation;
SHOW default_transaction_read_only;
SHOW default_transaction_deferrable;
SELECT name, setting, reset_val, source
FROM pg_settings
WHERE name IN ('default_transaction_isolation',
               'default_transaction_read_only',
               'default_transaction_deferrable',
               'transaction_isolation',
               'transaction_read_only',
               'transaction_deferrable')
ORDER BY name;

BEGIN;
SHOW transaction_isolation;
SHOW transaction_read_only;
SHOW transaction_deferrable;
SET TRANSACTION READ WRITE, NOT DEFERRABLE;
SELECT current_setting('transaction_isolation'),
       current_setting('transaction_read_only'),
       current_setting('transaction_deferrable');
SELECT name, setting, reset_val, source
FROM pg_settings
WHERE name IN ('transaction_isolation',
               'transaction_read_only',
               'transaction_deferrable')
ORDER BY name;
ROLLBACK;

SET default_transaction_isolation = 'read uncommitted';
SET default_transaction_read_only = off;
SET default_transaction_deferrable = off;
BEGIN;
SHOW transaction_isolation;
SET transaction_isolation = 'repeatable read';
SET transaction_read_only = on;
SET transaction_deferrable = on;
SHOW transaction_isolation;
SHOW transaction_read_only;
SHOW transaction_deferrable;
SELECT current_setting('transaction_isolation'),
       current_setting('transaction_read_only'),
       current_setting('transaction_deferrable');
ROLLBACK;

BEGIN;
RESET TRANSACTION ISOLATION LEVEL;
ROLLBACK;
BEGIN;
RESET transaction_read_only;
ROLLBACK;
BEGIN;
RESET transaction_deferrable;
ROLLBACK;
BEGIN READ ONLY;
RESET ALL;
SHOW transaction_read_only;
ROLLBACK;

BEGIN;
SAVEPOINT characteristics;
SET TRANSACTION READ ONLY;
SHOW transaction_read_only;
ROLLBACK TO characteristics;
SHOW transaction_read_only;
SELECT source FROM pg_settings WHERE name = 'transaction_read_only';
ROLLBACK;
BEGIN;
SAVEPOINT characteristics;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;
ROLLBACK;
BEGIN;
SAVEPOINT characteristics;
SET TRANSACTION DEFERRABLE;
ROLLBACK;

BEGIN;
SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY;
ROLLBACK;
SHOW default_transaction_read_only;

SET SCHEMA 'pg_catalog';
SHOW search_path;
SET application_name = 'transaction-differential';
SET application_name FROM CURRENT;
SHOW application_name;
SET NAMES;
SHOW client_encoding;

SET seed TO 0.5;
SELECT random();
SELECT setseed(0.5);
SELECT random();
SELECT set_config('seed', '0.5', false);
SELECT random();
SHOW seed;
RESET seed;
SET seed FROM CURRENT;

SET TIME ZONE +2;
SHOW TIME ZONE;
SET TIME ZONE INTERVAL '-03:30';
SHOW TIME ZONE;
SET TIME ZONE INTERVAL '2:30' HOUR TO MINUTE;
SHOW TIME ZONE;
SET TIME ZONE INTERVAL(0) '1:45';
SHOW TIME ZONE;
SET TIME ZONE LOCAL;
SHOW TIME ZONE;
SET TIME ZONE +4;
RESET TIME ZONE;
SHOW TIME ZONE;
SET XML OPTION CONTENT;
SHOW xmloption;
SET CATALOG 'other';
RESET ALL;
