-- Groupings that a retained reader computes itself (a routine call in the
-- grouped output or HAVING hands off on its group's row) match PostgreSQL:
-- every aggregate kind, HAVING, global aggregates, bounds and errors.
CREATE TABLE grouped_orders(id int, customer int, amount numeric, note text);
INSERT INTO grouped_orders SELECT g, g % 4, g * 1.5, 'n' || g FROM generate_series(1, 20) g;
INSERT INTO grouped_orders VALUES (21, NULL, NULL, NULL), (22, 2, 0, 'n22');
CREATE FUNCTION grouped_label(i bigint) RETURNS text LANGUAGE sql AS $$ SELECT 'c' || i $$;
CREATE AGGREGATE grouped_total(int) (sfunc = int4pl, stype = int, initcond = '0');
SELECT customer, grouped_label(customer), count(*), sum(amount), avg(id), min(note), max(note)
  FROM grouped_orders GROUP BY customer ORDER BY customer;
SELECT customer, grouped_label(count(*)), string_agg(note, ',' ORDER BY id DESC),
  array_agg(id ORDER BY id) FILTER (WHERE id % 2 = 0), count(DISTINCT id % 3),
  grouped_total(id), percentile_cont(0.5) WITHIN GROUP (ORDER BY amount),
  mode() WITHIN GROUP (ORDER BY id % 2), bool_or(id > 18), json_agg(id ORDER BY id),
  stddev_samp(id), corr(id, amount)
  FROM grouped_orders GROUP BY customer HAVING count(*) > 1 ORDER BY customer;
SELECT grouped_label(count(*)), sum(id), max(amount) FROM grouped_orders;
SELECT grouped_label(count(*)), sum(id) FROM grouped_orders WHERE id < 0;
SELECT customer, grouped_label(customer) FROM grouped_orders GROUP BY customer
  HAVING grouped_label(customer) <> 'c1' ORDER BY customer;
SELECT customer % 2 AS parity, grouped_label(sum(id)) FROM grouped_orders
  GROUP BY 1 ORDER BY 1 LIMIT 1 OFFSET 1;
SELECT o.customer, grouped_label(count(v.x)) FROM grouped_orders o
  LEFT JOIN (VALUES (1), (2)) v(x) ON v.x = o.customer GROUP BY o.customer
  ORDER BY o.customer;
SELECT g % 3 AS k, grouped_label(sum(g)), array_agg(DISTINCT g % 2)
  FROM generate_series(1, 10) g GROUP BY g % 3 ORDER BY g % 3;
SELECT grouped_label(id), count(*) FROM grouped_orders GROUP BY customer;
SELECT customer, grouped_label(sum(1 / (id - 5))) FROM grouped_orders
  GROUP BY customer ORDER BY customer;
SELECT customer, grouped_label(customer) FROM grouped_orders GROUP BY customer
  HAVING sum(id) ORDER BY customer;
SELECT customer, grouped_label(count(*)) FROM grouped_orders GROUP BY customer
  HAVING sum(id) / (customer - 1) > 0 ORDER BY customer;
SELECT customer, grouped_label(count(*)) FROM grouped_orders
  JOIN (VALUES (1), (2)) v(customer) USING (customer) GROUP BY customer ORDER BY customer;
