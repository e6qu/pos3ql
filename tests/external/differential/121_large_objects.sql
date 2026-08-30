SELECT lo_create(92001::oid);
SELECT lo_put(92001::oid, 4098::bigint, decode('7879', 'hex'));
SELECT octet_length(lo_get(92001::oid)),
       encode(lo_get(92001::oid, 4094::bigint, 6), 'hex');

BEGIN;
SELECT lo_open(92001::oid, 131072);
SELECT lo_lseek64(0, 7000::bigint, 0), lo_truncate64(0, 5000::bigint);
SAVEPOINT large_object_changed;
SELECT lo_lseek64(0, 0::bigint, 0), lowrite(0, decode('61', 'hex'));
ROLLBACK TO large_object_changed;
SELECT lo_tell64(0), encode(lo_get(92001::oid, 0::bigint, 1), 'hex');
COMMIT;

SELECT pageno, octet_length(data)
  FROM pg_largeobject WHERE loid = 92001::oid ORDER BY pageno;
CREATE ROLE large_object_reader;
GRANT SELECT ON LARGE OBJECT 92001 TO large_object_reader;
COMMENT ON LARGE OBJECT 92001 IS 'differential large object';
SELECT oid, lomacl IS NOT NULL, obj_description(oid, 'pg_largeobject')
  FROM pg_largeobject_metadata WHERE oid = 92001::oid;
ALTER LARGE OBJECT 92001 OWNER TO large_object_reader;
SELECT metadata.oid, metadata.lomowner = roles.oid
  FROM pg_largeobject_metadata metadata
  JOIN pg_roles roles ON roles.rolname = 'large_object_reader'
 WHERE metadata.oid = 92001::oid;

SELECT relname, relnatts, relhasindex
  FROM pg_class WHERE oid IN (2613, 2995) ORDER BY oid;
SELECT relname, relnatts, relam
  FROM pg_class WHERE oid IN (2683, 2996) ORDER BY oid;
SELECT indexrelid, indrelid, indisprimary, indisunique, indkey::text, indclass::text
  FROM pg_index WHERE indrelid IN (2613, 2995) ORDER BY indexrelid;
SELECT attrelid, attname, atttypid, attnotnull
  FROM pg_attribute
 WHERE attrelid IN (2613, 2683, 2995, 2996) AND attnum > 0
 ORDER BY attrelid, attnum;
SELECT schemaname, tablename, indexname, indexdef
  FROM pg_indexes
 WHERE indexname IN ('pg_largeobject_loid_pn_index',
                     'pg_largeobject_metadata_oid_index')
 ORDER BY indexname;
SELECT lo_unlink(92001::oid);
DROP ROLE large_object_reader;

CREATE TABLE large_object_oid_history (oid oid);
INSERT INTO large_object_oid_history VALUES (lo_create(0));
SELECT lo_unlink(oid) FROM large_object_oid_history;
INSERT INTO large_object_oid_history VALUES (lo_create(0));
SELECT count(*) = count(DISTINCT oid) FROM large_object_oid_history;
SELECT lo_unlink((SELECT oid FROM large_object_oid_history
                   ORDER BY oid DESC LIMIT 1));
DROP TABLE large_object_oid_history;

SELECT lo_create(4294967295::oid);
COMMENT ON LARGE OBJECT 4294967295 IS 'unsigned identity';
SELECT objoid, classoid, obj_description(objoid, 'pg_largeobject')
  FROM pg_description
 WHERE classoid = 2613::oid AND objoid = 4294967295::oid;
SELECT lo_unlink(4294967295::oid);
