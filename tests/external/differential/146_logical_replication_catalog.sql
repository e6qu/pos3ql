-- PostgreSQL 18 logical-replication SQL and monitoring identities. Runtime
-- slot positions are covered by the wire and native-tool suites; this corpus
-- keeps the stable catalog and pg_lsn contracts differential.

SELECT oid, typname, typlen, typcategory, typarray, typelem
  FROM pg_type
 WHERE oid IN (3220, 3221)
 ORDER BY oid;

SELECT oid, proname, pronargs, pronargdefaults, prorettype, proretset,
       proisstrict, provolatile, proparallel, proargtypes::text
  FROM pg_proc
 WHERE oid IN (3577, 3578, 3780, 3786, 3878, 4222, 4223, 4224, 6170, 6232)
 ORDER BY oid;

SELECT relation.relname, attribute.attnum, attribute.attname,
       attribute.atttypid, attribute.atttypmod
  FROM pg_class relation
  JOIN pg_attribute attribute ON attribute.attrelid = relation.oid
 WHERE relation.oid IN (12231, 12248, 12261, 12266, 12347)
   AND attribute.attnum > 0
   AND NOT attribute.attisdropped
 ORDER BY relation.oid, attribute.attnum;

SELECT count(*) FROM pg_replication_slots;
SELECT count(*) FROM pg_stat_replication;
SELECT count(*) FROM pg_stat_replication_slots;
SELECT count(*) FROM pg_stat_subscription;
SELECT count(*) FROM pg_stat_subscription_stats;

CREATE TABLE logical_replication_lsn_values (
  position pg_lsn PRIMARY KEY,
  positions pg_lsn[] NOT NULL
);
INSERT INTO logical_replication_lsn_values VALUES
  ('1/10', ARRAY['1/20'::pg_lsn, 'FFFFFFFF/FFFFFFFF'::pg_lsn]),
  ('0/FF', ARRAY[]::pg_lsn[]);
SELECT position, positions::text, pg_typeof(position), pg_typeof(positions)
  FROM logical_replication_lsn_values
 ORDER BY position;
SELECT indclass::text
  FROM pg_index
 WHERE indexrelid = 'logical_replication_lsn_values_pkey'::regclass;
INSERT INTO logical_replication_lsn_values VALUES ('1/10', '{}');
DROP TABLE logical_replication_lsn_values;
