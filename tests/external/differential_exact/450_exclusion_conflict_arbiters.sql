-- ON CONFLICT DO NOTHING treats exclusion constraints as arbiters; DO UPDATE
-- and deferrable constraints cannot arbitrate.
CREATE TABLE oc450 (r int4range, a int,
  CONSTRAINT oc450_x EXCLUDE USING gist (r WITH &&) WHERE (a > 0));
INSERT INTO oc450 VALUES ('[1,5)', 1);
INSERT INTO oc450 VALUES ('[2,3)', 1) ON CONFLICT DO NOTHING;
INSERT INTO oc450 VALUES ('[2,3)', 0) ON CONFLICT DO NOTHING;
INSERT INTO oc450 VALUES ('[2,3)', 1), ('[7,8)', 1), ('[7,9)', 1)
  ON CONFLICT ON CONSTRAINT oc450_x DO NOTHING;
INSERT INTO oc450 VALUES ('[2,3)', 1) ON CONFLICT ON CONSTRAINT oc450_x DO UPDATE SET a = 5;
INSERT INTO oc450 VALUES ('[2,3)', 1) ON CONFLICT (r) DO NOTHING;
SELECT * FROM oc450 ORDER BY r, a;
CREATE TABLE oc450_deferred (r int4range,
  CONSTRAINT oc450_deferred_x EXCLUDE USING gist (r WITH &&) DEFERRABLE);
INSERT INTO oc450_deferred VALUES ('[1,5)');
INSERT INTO oc450_deferred VALUES ('[2,3)') ON CONFLICT DO NOTHING;
INSERT INTO oc450_deferred VALUES ('[2,3)') ON CONFLICT ON CONSTRAINT oc450_deferred_x DO NOTHING;
CREATE TABLE oc450_key (a int, CONSTRAINT oc450_key_a UNIQUE (a) DEFERRABLE);
INSERT INTO oc450_key VALUES (1) ON CONFLICT DO NOTHING;
INSERT INTO oc450_key VALUES (1) ON CONFLICT ON CONSTRAINT oc450_key_a DO NOTHING;
CREATE TABLE oc450_expression (a int, b int,
  CONSTRAINT oc450_expression_x EXCLUDE USING gist (int4range(a, b) WITH &&));
INSERT INTO oc450_expression VALUES (1, 5);
INSERT INTO oc450_expression VALUES (2, 3), (6, 7) ON CONFLICT DO NOTHING RETURNING *;
INSERT INTO oc450_expression VALUES (6, 9), (9, 10)
  ON CONFLICT ON CONSTRAINT oc450_expression_x DO NOTHING RETURNING *;
-- A partition's clone arbitrates for its parent's constraint.
CREATE TABLE oc450_parts (p int4range, a int, b int,
  CONSTRAINT oc450_parts_x EXCLUDE USING gist (p WITH =, int4range(a, b) WITH &&))
  PARTITION BY LIST (p);
CREATE TABLE oc450_part1 PARTITION OF oc450_parts FOR VALUES IN ('[1,2)');
INSERT INTO oc450_parts VALUES ('[1,2)', 1, 5);
INSERT INTO oc450_parts VALUES ('[1,2)', 2, 3), ('[1,2)', 5, 6)
  ON CONFLICT ON CONSTRAINT oc450_parts_x DO NOTHING RETURNING *;
SELECT * FROM oc450_parts ORDER BY a;
DROP TABLE oc450, oc450_deferred, oc450_key, oc450_expression, oc450_parts;
