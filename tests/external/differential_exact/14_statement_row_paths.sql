-- One order keeps its values through routing, conflict updates and cascades.
CREATE TABLE statement_row_customers (id integer PRIMARY KEY);
INSERT INTO statement_row_customers VALUES (1);
CREATE TABLE statement_row_orders (
    id integer,
    customer integer REFERENCES statement_row_customers(id) ON UPDATE CASCADE ON DELETE CASCADE,
    region integer,
    total integer DEFAULT 5,
    PRIMARY KEY (id, region)
) PARTITION BY RANGE (region);
CREATE TABLE statement_row_near PARTITION OF statement_row_orders FOR VALUES FROM (0) TO (10);
CREATE TABLE statement_row_far PARTITION OF statement_row_orders FOR VALUES FROM (10) TO (30);
INSERT INTO statement_row_orders VALUES (1, 1, 2, 5), (2, 1, 12, 6);
INSERT INTO statement_row_orders VALUES (1, 1, 2, 9)
    ON CONFLICT (id, region) DO UPDATE SET total = EXCLUDED.total;
UPDATE statement_row_orders SET region = 20, total = total + 1 WHERE id = 1;
UPDATE statement_row_customers SET id = 7 WHERE id = 1;
SELECT id, customer, region, total FROM statement_row_orders ORDER BY id;

-- Undo a cascading delete without changing either physical partition's values.
BEGIN;
SAVEPOINT before_cascade;
DELETE FROM statement_row_customers WHERE id = 7;
ROLLBACK TO before_cascade;
RELEASE before_cascade;
COMMIT;
SELECT id, customer, region, total FROM statement_row_orders ORDER BY id;

-- Refresh and rollback of TRUNCATE use the same checked row installation path.
CREATE MATERIALIZED VIEW statement_row_summary AS
    SELECT id, region, total FROM statement_row_orders;
UPDATE statement_row_orders SET total = total + 2 WHERE id = 2;
REFRESH MATERIALIZED VIEW statement_row_summary;
SELECT id, region, total FROM statement_row_summary ORDER BY id;
BEGIN;
TRUNCATE statement_row_customers CASCADE;
ROLLBACK;
SELECT count(*) FROM statement_row_orders;
