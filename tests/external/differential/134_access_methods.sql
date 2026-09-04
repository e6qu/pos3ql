-- Database-local table access methods, catalog identity, and dependency drop.

SELECT amname, amhandler, amtype
FROM pg_am
WHERE amname IN ('heap', 'btree', 'hash', 'gist', 'gin', 'brin', 'spgist')
ORDER BY amname;

CREATE ACCESS METHOD differential_heap TYPE TABLE HANDLER heap_tableam_handler;
CREATE TABLE differential_access_method_rows (id integer) USING differential_heap;
INSERT INTO differential_access_method_rows VALUES (7);

SELECT a.amname, a.amhandler = 3, a.amtype, r.id
FROM pg_class c
JOIN pg_am a ON a.oid = c.relam
JOIN differential_access_method_rows r ON true
WHERE c.relname = 'differential_access_method_rows';

COMMENT ON ACCESS METHOD differential_heap IS 'differential table handler';
SELECT obj_description(oid, 'pg_am')
FROM pg_am
WHERE amname = 'differential_heap';

DROP ACCESS METHOD differential_heap CASCADE;
SELECT count(*) FROM pg_am WHERE amname = 'differential_heap';
SELECT count(*) FROM pg_class WHERE relname = 'differential_access_method_rows';
