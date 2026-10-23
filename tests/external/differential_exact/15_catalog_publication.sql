CREATE DOMAIN catalog_customer_id AS integer;
CREATE DOMAIN catalog_unused_domain AS integer;
CREATE TYPE catalog_pair AS (value integer);
CREATE TYPE catalog_state AS ENUM ('ready');
CREATE TYPE catalog_unused_pair AS (value integer);
CREATE SCHEMA catalog_empty;
CREATE TABLE catalog_orders (id catalog_customer_id PRIMARY KEY);
CREATE TABLE catalog_invoices (id integer);
INSERT INTO catalog_orders VALUES (1);
INSERT INTO catalog_invoices VALUES (22);

-- Changes while another connection retains unrelated work.
ALTER DOMAIN catalog_customer_id RENAME TO catalog_account_id;
ALTER DOMAIN catalog_account_id SET DEFAULT 9;
ALTER DOMAIN catalog_account_id ADD CONSTRAINT catalog_positive CHECK (VALUE > 0);
ALTER TYPE catalog_pair RENAME ATTRIBUTE value TO total;
ALTER TYPE catalog_pair ADD ATTRIBUTE note text;
ALTER TYPE catalog_state RENAME TO catalog_status;
DROP TYPE catalog_unused_pair;
DROP DOMAIN catalog_unused_domain;
DROP SCHEMA catalog_empty;
SELECT id FROM catalog_orders;
SELECT id FROM catalog_invoices;
SELECT 9::catalog_account_id;
SELECT 'ready'::catalog_status;
SELECT ROW(7, 'ok')::catalog_pair;
