-- Declarative partitioning: definitions, routing, parent reads and writes.
CREATE TABLE partition_range (id integer PRIMARY KEY, note text) PARTITION BY RANGE (id);
CREATE TABLE partition_range_low PARTITION OF partition_range FOR VALUES FROM (MINVALUE) TO (0);
CREATE TABLE partition_range_mid PARTITION OF partition_range FOR VALUES FROM (0) TO (10);
CREATE TABLE partition_range_other PARTITION OF partition_range DEFAULT;
INSERT INTO partition_range VALUES (-1, 'low'), (1, 'one'), (9, 'nine'), (10, 'other');
SELECT id, note FROM partition_range ORDER BY id;
UPDATE partition_range SET id = 11, note = 'moved' WHERE id = 9;
DELETE FROM partition_range WHERE id = -1;
SELECT id, note FROM partition_range ORDER BY id;

CREATE TABLE partition_list (id integer PRIMARY KEY, note text) PARTITION BY LIST (id);
CREATE TABLE partition_list_one PARTITION OF partition_list FOR VALUES IN (1, 3);
CREATE TABLE partition_list_other PARTITION OF partition_list DEFAULT;
INSERT INTO partition_list VALUES (1, 'selected'), (2, 'default'), (3, 'selected');
SELECT id, note FROM partition_list ORDER BY id;

CREATE TABLE partition_hash (id integer PRIMARY KEY) PARTITION BY HASH (id);
CREATE TABLE partition_hash_zero PARTITION OF partition_hash FOR VALUES WITH (MODULUS 2, REMAINDER 0);
CREATE TABLE partition_hash_one PARTITION OF partition_hash FOR VALUES WITH (MODULUS 2, REMAINDER 1);
INSERT INTO partition_hash VALUES (1), (2), (3), (4);
SELECT id FROM partition_hash ORDER BY id;
