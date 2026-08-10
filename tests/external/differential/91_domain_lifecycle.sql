-- Domain identity moves preserve dependent columns and child domains.
DROP TABLE IF EXISTS domain_lifecycle_values;
DROP DOMAIN IF EXISTS domain_lifecycle_child;
DROP DOMAIN IF EXISTS domain_lifecycle_target.domain_lifecycle_new;
DROP DOMAIN IF EXISTS domain_lifecycle_old;
DROP SCHEMA IF EXISTS domain_lifecycle_target;

CREATE SCHEMA domain_lifecycle_target;
CREATE DOMAIN domain_lifecycle_old AS integer CHECK (VALUE > 0);
CREATE DOMAIN domain_lifecycle_child AS domain_lifecycle_old;
CREATE TABLE domain_lifecycle_values (
  value domain_lifecycle_old,
  values domain_lifecycle_old[]
);
INSERT INTO domain_lifecycle_values VALUES (3, ARRAY[4, 5]::domain_lifecycle_old[]);

BEGIN;
ALTER DOMAIN domain_lifecycle_old RENAME TO domain_lifecycle_new;
SELECT typname, nspname
  FROM pg_type JOIN pg_namespace ON typnamespace = pg_namespace.oid
 WHERE typname IN ('domain_lifecycle_old', 'domain_lifecycle_new')
 ORDER BY typname;
ROLLBACK;
SELECT typname, nspname
  FROM pg_type JOIN pg_namespace ON typnamespace = pg_namespace.oid
 WHERE typname IN ('domain_lifecycle_old', 'domain_lifecycle_new')
 ORDER BY typname;

ALTER DOMAIN domain_lifecycle_old RENAME TO domain_lifecycle_new;
ALTER DOMAIN domain_lifecycle_new SET SCHEMA domain_lifecycle_target;
INSERT INTO domain_lifecycle_values VALUES (7, ARRAY[8]::domain_lifecycle_target.domain_lifecycle_new[]);
SELECT value FROM domain_lifecycle_values ORDER BY value;
SELECT 9::domain_lifecycle_child;
SELECT 0::domain_lifecycle_child;
