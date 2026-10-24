CREATE TABLE key_orders_sql(id integer PRIMARY KEY, amount integer);
INSERT INTO key_orders_sql VALUES (1, 11), (2, 22);
DO $body$
BEGIN
  BEGIN
    EXECUTE 'CREATE INDEX key_orders_bad ON key_orders_sql ((id / (amount - 11)))';
    RAISE EXCEPTION 'index creation did not reject division by zero';
  EXCEPTION WHEN SQLSTATE '22012' THEN
    NULL;
  END;
END
$body$;
SELECT count(*) FROM pg_indexes WHERE tablename = 'key_orders_sql';
SELECT id, amount FROM key_orders_sql WHERE id = 1;
CREATE INDEX key_orders_expression ON key_orders_sql ((id + amount)) INCLUDE(amount) WHERE amount > 0;
UPDATE key_orders_sql SET id = 3, amount = 20 WHERE id = 1;
REINDEX INDEX key_orders_expression;
BEGIN;
DELETE FROM key_orders_sql WHERE id = 2;
ROLLBACK;
SELECT id, amount, id + amount FROM key_orders_sql WHERE id = 3;
SELECT count(*), sum(id), sum(amount) FROM key_orders_sql;
