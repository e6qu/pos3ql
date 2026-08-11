CREATE TABLE covering_index_rows (key integer, payload text, note text);
CREATE UNIQUE INDEX covering_index_key ON covering_index_rows (key) INCLUDE (payload, note);
INSERT INTO covering_index_rows VALUES (1, 'one', 'first');
SELECT indexdef FROM pg_indexes WHERE indexname = 'covering_index_key';
SELECT indnatts, indnkeyatts, indkey FROM pg_index index_catalog JOIN pg_class relation ON relation.oid = index_catalog.indexrelid WHERE relation.relname = 'covering_index_key';
INSERT INTO covering_index_rows VALUES (1, 'different', 'different');
