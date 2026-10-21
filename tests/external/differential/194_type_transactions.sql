-- Type names, rules and deletion follow savepoints and the final commit.
BEGIN;
CREATE DOMAIN type_tx_domain AS integer DEFAULT 1;
ALTER DOMAIN type_tx_domain SET DEFAULT 2;
SAVEPOINT keep_type;
ALTER DOMAIN type_tx_domain SET DEFAULT 3;
ALTER DOMAIN type_tx_domain RENAME TO type_tx_domain_pending;
DROP DOMAIN type_tx_domain_pending;
SELECT count(*) FROM pg_type WHERE typname = 'type_tx_domain_pending';
ROLLBACK TO keep_type;
ALTER DOMAIN type_tx_domain SET DEFAULT 4;
COMMIT;
CREATE TABLE type_tx_rows(value type_tx_domain);
INSERT INTO type_tx_rows DEFAULT VALUES;
SELECT value FROM type_tx_rows;

BEGIN;
CREATE TYPE type_tx_enum AS ENUM ('before');
ALTER TYPE type_tx_enum RENAME VALUE 'before' TO 'first';
SAVEPOINT keep_type;
ALTER TYPE type_tx_enum RENAME VALUE 'first' TO 'discarded';
ALTER TYPE type_tx_enum RENAME TO type_tx_enum_pending;
DROP TYPE type_tx_enum_pending;
SELECT count(*) FROM pg_type WHERE typname = 'type_tx_enum_pending';
ROLLBACK TO keep_type;
ALTER TYPE type_tx_enum RENAME VALUE 'first' TO 'final';
COMMIT;
SELECT 'final'::type_tx_enum::text;

BEGIN;
CREATE TYPE type_tx_record AS (before integer);
ALTER TYPE type_tx_record RENAME ATTRIBUTE before TO first;
SAVEPOINT keep_type;
ALTER TYPE type_tx_record RENAME ATTRIBUTE first TO discarded;
ALTER TYPE type_tx_record RENAME TO type_tx_record_pending;
DROP TYPE type_tx_record_pending;
SELECT count(*) FROM pg_type WHERE typname = 'type_tx_record_pending';
ROLLBACK TO keep_type;
ALTER TYPE type_tx_record RENAME ATTRIBUTE first TO final;
COMMIT;
SELECT (ROW(9)::type_tx_record).final;

-- A pending rename must preserve dependency checks and revalidation.
CREATE DOMAIN type_tx_parent AS integer;
CREATE DOMAIN type_tx_child AS type_tx_parent;
CREATE TABLE type_tx_children(value type_tx_child);
INSERT INTO type_tx_children VALUES (1);
BEGIN;
ALTER DOMAIN type_tx_parent RENAME TO type_tx_parent_moved;
SAVEPOINT before_check;
ALTER DOMAIN type_tx_parent_moved ADD CHECK (VALUE > 10);
ROLLBACK TO before_check;
SAVEPOINT before_drop;
DROP DOMAIN type_tx_parent_moved;
ROLLBACK TO before_drop;
DROP DOMAIN type_tx_parent_moved CASCADE;
SELECT count(*) FROM pg_attribute
 WHERE attrelid = 'type_tx_children'::regclass AND attnum > 0 AND NOT attisdropped;
ROLLBACK;
SELECT value FROM type_tx_children;

-- Moving a type out of a schema must preserve its outside table column.
CREATE SCHEMA type_tx_source;
CREATE SCHEMA type_tx_target;
CREATE DOMAIN type_tx_source.value AS integer;
CREATE TABLE type_tx_outside(value type_tx_source.value);
INSERT INTO type_tx_outside VALUES (5);
BEGIN;
ALTER DOMAIN type_tx_source.value SET SCHEMA type_tx_target;
DROP SCHEMA type_tx_source CASCADE;
SELECT value FROM type_tx_outside;
COMMIT;
SELECT value FROM type_tx_outside;

BEGIN;
CREATE DOMAIN type_tx_transient AS integer;
DROP DOMAIN type_tx_transient;
CREATE TYPE type_tx_transient AS ENUM ('ready');
DROP TYPE type_tx_transient;
CREATE TYPE type_tx_transient AS (value integer);
DROP TYPE type_tx_transient;
COMMIT;
SELECT count(*) FROM pg_type WHERE typname = 'type_tx_transient';

DROP TABLE type_tx_outside;
DROP SCHEMA type_tx_target CASCADE;
DROP TABLE type_tx_children;
DROP DOMAIN type_tx_child;
DROP DOMAIN type_tx_parent;
DROP TABLE type_tx_rows;
DROP DOMAIN type_tx_domain;
DROP TYPE type_tx_record;
DROP TYPE type_tx_enum;
