-- oid is unsigned, and its standard array identity must retain the full range.
CREATE TABLE diff_oid_arrays (values oid[]);
INSERT INTO diff_oid_arrays VALUES
  (ARRAY[1::oid, NULL, 4294967295::oid]),
  (ARRAY[]::oid[]),
  (NULL);
SELECT pg_typeof(values), values::text FROM diff_oid_arrays
ORDER BY values::text COLLATE "C" NULLS LAST;
SELECT 4294967295::oid, 4294967295::oid::text, 4294967295::oid = 4294967295::oid;
DROP TABLE diff_oid_arrays;
