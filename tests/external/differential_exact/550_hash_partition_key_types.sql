-- HASH partitioning routes rows with each key type's default hash operator
-- class, skipping null keys, as satisfies_hash_partition checks them.
CREATE TABLE ht (k text, b char(5), d date, ts timestamp, f float8, fl bool, u uuid, by bytea, v varchar(10), t time, f4 real, o oid, c "char", tz timestamptz) PARTITION BY HASH (k);
CREATE TABLE ht0 PARTITION OF ht FOR VALUES WITH (MODULUS 3, REMAINDER 0);
CREATE TABLE ht1 PARTITION OF ht FOR VALUES WITH (MODULUS 3, REMAINDER 1);
CREATE TABLE ht2 PARTITION OF ht FOR VALUES WITH (MODULUS 3, REMAINDER 2);
INSERT INTO ht (k) SELECT 'key' || i FROM generate_series(1, 30) i;
INSERT INTO ht (k) VALUES (NULL), ('');
SELECT tableoid::regclass, count(*), string_agg(coalesce(k, 'null'), ',' ORDER BY k) FROM ht GROUP BY 1 ORDER BY 1;
SELECT regexp_replace(pg_get_partition_constraintdef('ht1'::regclass), '''\d+''::oid', 'OID');
CREATE TABLE hm (b char(5), d date, ts timestamp, f float8, fl bool, u uuid, by bytea, v varchar(10), t time, f4 real, o oid, c "char", tz timestamptz) PARTITION BY HASH (b, d, ts, f, fl, u, by, v, t, f4, o, c, tz);
CREATE TABLE hm0 PARTITION OF hm FOR VALUES WITH (MODULUS 4, REMAINDER 0);
CREATE TABLE hm1 PARTITION OF hm FOR VALUES WITH (MODULUS 4, REMAINDER 1);
CREATE TABLE hm2 PARTITION OF hm FOR VALUES WITH (MODULUS 4, REMAINDER 2);
CREATE TABLE hm3 PARTITION OF hm FOR VALUES WITH (MODULUS 4, REMAINDER 3);
SET timezone = 'UTC';
INSERT INTO hm SELECT 'b' || i, date '2024-01-01' + i, timestamp '2024-01-01' + i * interval '1 hour', i / 3.0, i % 2 = 0,
  ('00000000-0000-0000-0000-0000000000' || lpad(i::text, 2, '0'))::uuid, ('\x' || lpad(to_hex(i), 4, '0'))::bytea, 'v' || i,
  time '10:00' + i * interval '1 minute', (i * 0.5)::real, i::oid, chr(65 + i)::"char", timestamptz '2024-01-01 00:00+00' + i * interval '1 day'
  FROM generate_series(1, 24) i;
INSERT INTO hm (b, f, f4) VALUES ('zero', 0.0, -0.0), ('nan', 'NaN', 'NaN'), (NULL, NULL, NULL);
SELECT tableoid::regclass, count(*), string_agg(coalesce(b, 'null'), ',' ORDER BY b) FROM hm GROUP BY 1 ORDER BY 1;
SELECT regexp_replace(pg_get_partition_constraintdef('hm2'::regclass), '''\d+''::oid', 'OID');
SELECT count(*) FROM hm WHERE satisfies_hash_partition('hm'::regclass, 4, 2, b, d, ts, f, fl, u, by, v, t, f4, o, c, tz);
DROP TABLE ht, hm;
CREATE TABLE hs (k text, i int) PARTITION BY HASH (k, i);
CREATE TABLE hs0 PARTITION OF hs FOR VALUES WITH (MODULUS 2, REMAINDER 0);
CREATE TABLE hs1 PARTITION OF hs FOR VALUES WITH (MODULUS 2, REMAINDER 1);
SELECT satisfies_hash_partition('hs'::regclass, 2, 0, 'a'::text, 1), satisfies_hash_partition('hs'::regclass, 2, 1, 'a'::text, 1);
SELECT satisfies_hash_partition('hs'::regclass, 2, 0, NULL::text, NULL::int), satisfies_hash_partition(NULL, 2, 0, 'a'::text, 1);
SELECT satisfies_hash_partition('hs'::regclass, 0, 0, 'a'::text, 1);
SELECT satisfies_hash_partition('hs'::regclass, 2, -1, 'a'::text, 1);
SELECT satisfies_hash_partition('hs'::regclass, 2, 2, 'a'::text, 1);
SELECT satisfies_hash_partition('hs'::regclass, 2, 0, 'a'::text);
SELECT satisfies_hash_partition('hs'::regclass, 2, 0, 1, 'a'::text);
SELECT satisfies_hash_partition('hs0'::regclass, 2, 0, 'a'::text, 1);
SELECT satisfies_hash_partition('hs'::regclass, 2, 0, 'a'::varchar, 1);
DROP TABLE hs;
CREATE TABLE hn (n numeric, m numeric(10,2)) PARTITION BY HASH (n, m);
CREATE TABLE hn0 PARTITION OF hn FOR VALUES WITH (MODULUS 5, REMAINDER 0);
CREATE TABLE hn1 PARTITION OF hn FOR VALUES WITH (MODULUS 5, REMAINDER 1);
CREATE TABLE hn2 PARTITION OF hn FOR VALUES WITH (MODULUS 5, REMAINDER 2);
CREATE TABLE hn3 PARTITION OF hn FOR VALUES WITH (MODULUS 5, REMAINDER 3);
CREATE TABLE hn4 PARTITION OF hn FOR VALUES WITH (MODULUS 5, REMAINDER 4);
INSERT INTO hn SELECT (i * 1234.5678 - 9000)::numeric, i * 1.5 FROM generate_series(1, 40) i;
INSERT INTO hn VALUES (0, 0), ('NaN', 1), ('Infinity', NULL), ('-Infinity', -3), (100000000, 0.01), (1.000, 10000), (-0.00010, 2);
SELECT tableoid::regclass, count(*), string_agg(n::text, ',' ORDER BY n) FROM hn GROUP BY 1 ORDER BY 1;
DROP TABLE hn;
