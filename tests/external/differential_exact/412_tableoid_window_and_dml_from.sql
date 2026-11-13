-- tableoid names the leaf table a row came from also in window queries and
-- in the FROM items of UPDATE ... FROM and DELETE ... USING.
CREATE TABLE dt_part (k int, v text) PARTITION BY RANGE (k);
CREATE TABLE dt_part_lo PARTITION OF dt_part FOR VALUES FROM (0) TO (10);
CREATE TABLE dt_part_hi PARTITION OF dt_part FOR VALUES FROM (10) TO (100);
INSERT INTO dt_part VALUES (1, 'a'), (11, 'b');
CREATE TABLE dt_parent (id int);
CREATE TABLE dt_child () INHERITS (dt_parent);
INSERT INTO dt_parent VALUES (1); INSERT INTO dt_child VALUES (11);
CREATE TABLE dt_target (k int, src text);
INSERT INTO dt_target VALUES (1, NULL), (11, NULL);
SELECT tableoid::regclass, k, row_number() OVER (ORDER BY k) FROM dt_part ORDER BY k;
SELECT k, first_value(tableoid::regclass) OVER (PARTITION BY tableoid ORDER BY k) FROM dt_part ORDER BY k;
SELECT tableoid::regclass, id, count(*) OVER () FROM dt_parent ORDER BY id;
UPDATE dt_target t SET src = p.tableoid::regclass::text FROM dt_part p WHERE p.k = t.k RETURNING t.k, t.src, p.tableoid::regclass;
UPDATE dt_target t SET src = p.tableoid::regclass::text FROM dt_parent p WHERE p.id = t.k RETURNING t.k, t.src, p.tableoid::regclass;
DELETE FROM dt_target t USING dt_part p WHERE p.k = t.k AND p.tableoid = 'dt_part_hi'::regclass RETURNING t.k, p.tableoid::regclass;
SELECT * FROM dt_target ORDER BY k;
SELECT k, tableoid::regclass, random() < 2 FROM dt_part ORDER BY k;
SELECT DISTINCT tableoid::regclass FROM dt_part ORDER BY 1;
DROP TABLE dt_part, dt_parent, dt_target CASCADE;
