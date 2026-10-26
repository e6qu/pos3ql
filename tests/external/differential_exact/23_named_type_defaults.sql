CREATE SCHEMA type_default_scope;
CREATE DOMAIN type_default_scope.quantity AS integer CHECK (VALUE > 0);
CREATE TYPE type_default_scope.status AS ENUM ('new', 'paid');
CREATE TYPE type_default_scope.item AS (id integer, status type_default_scope.status, quantity type_default_scope.quantity);
CREATE DOMAIN type_default_scope.ordered_status AS type_default_scope.status CHECK (VALUE::text <> 'new');
CREATE DOMAIN type_default_scope.approved_item AS type_default_scope.item CHECK ((VALUE).quantity > 0);
CREATE TABLE type_default_scope.orders (
  quantity type_default_scope.quantity DEFAULT 3,
  status type_default_scope.status DEFAULT 'paid',
  statuses type_default_scope.status[] DEFAULT ARRAY['paid'::type_default_scope.status],
  quantities type_default_scope.quantity[] DEFAULT ARRAY[3::type_default_scope.quantity],
  ordered_statuses type_default_scope.ordered_status[] DEFAULT ARRAY['paid'::type_default_scope.ordered_status],
  items type_default_scope.item[] DEFAULT ARRAY[ROW(1, 'paid'::type_default_scope.status, 3::type_default_scope.quantity)::type_default_scope.item],
  approved_items type_default_scope.approved_item[] DEFAULT ARRAY[ROW(2, 'paid'::type_default_scope.status, 4::type_default_scope.quantity)::type_default_scope.approved_item]
);
INSERT INTO type_default_scope.orders DEFAULT VALUES;
SELECT quantity = 3 AND status::text = 'paid'
   AND statuses[1]::text = 'paid' AND quantities[1] = 3
   AND ordered_statuses[1]::text = 'paid'
   AND (items[1]).id = 1 AND (approved_items[1]).id = 2
  FROM type_default_scope.orders;
SELECT pg_typeof(statuses)::text = 'type_default_scope.status[]'
   AND pg_typeof(quantities)::text = 'type_default_scope.quantity[]'
   AND pg_typeof(ordered_statuses)::text = 'type_default_scope.ordered_status[]'
   AND pg_typeof(items)::text = 'type_default_scope.item[]'
   AND pg_typeof(approved_items)::text = 'type_default_scope.approved_item[]'
  FROM type_default_scope.orders;
DROP TABLE type_default_scope.orders;
DROP DOMAIN type_default_scope.approved_item, type_default_scope.ordered_status;
DROP TYPE type_default_scope.item, type_default_scope.status;
DROP DOMAIN type_default_scope.quantity;
DROP SCHEMA type_default_scope;
