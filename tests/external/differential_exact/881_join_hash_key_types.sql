-- Hash joins, hash aggregates and DISTINCT over keys of other hashable
-- types: each type's default hash support function (hash_numeric,
-- hashfloat8, timestamp_hash, uuid_hash, hashbpchar, interval_hash, ...)
-- orders the hash table, and its batches split by the same values.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/hash/hashfunc.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/numeric.c
SET TimeZone = 'UTC';
CREATE TABLE ta881 (id int, n numeric, f float8, r real, ts timestamp, tz timestamptz, u uuid, c char(4), vc varchar(10), iv interval, by bytea, tm time);
CREATE TABLE tb881 (id int, n numeric, f float8, r real, ts timestamp, tz timestamptz, u uuid, c char(4), vc varchar(10), iv interval, by bytea, tm time);
INSERT INTO ta881 SELECT g, (g % 23)::numeric / 4, (g % 19) * 0.5, (g % 17) * 0.25, timestamp '2024-01-01' + (g % 13) * interval '7 hours', timestamptz '2024-01-01 00:00+00' + (g % 11) * interval '5 hours', md5((g % 29)::text)::uuid, 'c' || (g % 7), 'v' || (g % 31), (g % 9) * interval '1 day 3 hours', decode(lpad(to_hex(g % 37), 4, '0'), 'hex'), time '00:00' + (g % 21) * interval '13 minutes' FROM generate_series(1, 600) g;
INSERT INTO tb881 SELECT g, (g % 31)::numeric / 4, (g % 23) * 0.5, (g % 13) * 0.25, timestamp '2024-01-01' + (g % 17) * interval '7 hours', timestamptz '2024-01-01 00:00+00' + (g % 7) * interval '5 hours', md5((g % 41)::text)::uuid, 'c' || (g % 5), 'v' || (g % 43), (g % 11) * interval '1 day 3 hours', decode(lpad(to_hex(g % 29), 4, '0'), 'hex'), time '00:00' + (g % 15) * interval '13 minutes' FROM generate_series(1, 500) g;
ANALYZE ta881;
ANALYZE tb881;
EXPLAIN SELECT ta881.n, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.n;
SELECT ta881.n, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.n;
SELECT ta881.f, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.f;
SELECT ta881.r, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.r;
SELECT ta881.ts, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.ts;
SELECT ta881.tz, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.tz;
SELECT ta881.u, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.u;
SELECT ta881.c, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.c;
SELECT ta881.vc, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.vc;
SELECT ta881.iv, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.iv;
SELECT ta881.by, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.by;
SELECT ta881.tm, count(*) FROM ta881 JOIN tb881 ON ta881.id = tb881.id GROUP BY ta881.tm;
EXPLAIN SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.n = tb881.n WHERE ta881.id < 40 AND tb881.id < 40;
SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.n = tb881.n WHERE ta881.id < 40 AND tb881.id < 40;
EXPLAIN SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.f = tb881.f WHERE ta881.id < 12;
SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.f = tb881.f WHERE ta881.id < 12;
EXPLAIN SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.ts = tb881.ts WHERE tb881.id < 12;
SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.ts = tb881.ts WHERE tb881.id < 12;
EXPLAIN SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.u = tb881.u WHERE ta881.id < 25;
SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.u = tb881.u WHERE ta881.id < 25;
EXPLAIN SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.c = tb881.c WHERE ta881.id < 20 AND tb881.id < 20;
SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.c = tb881.c WHERE ta881.id < 20 AND tb881.id < 20;
EXPLAIN SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.iv = tb881.iv WHERE ta881.id < 10;
SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.iv = tb881.iv WHERE ta881.id < 10;
SELECT DISTINCT ta881.n, tb881.r FROM ta881 JOIN tb881 ON ta881.id = tb881.id WHERE ta881.id < 100;
EXPLAIN SELECT count(*), count(ta881.id), count(tb881.id) FROM ta881 FULL JOIN tb881 ON ta881.vc = tb881.vc;
SELECT count(*), count(ta881.id), count(tb881.id) FROM ta881 FULL JOIN tb881 ON ta881.vc = tb881.vc;
SET work_mem = '64kB';
EXPLAIN SELECT ta881.id, tb881.id FROM ta881 JOIN tb881 ON ta881.n = tb881.n;
SELECT count(*), sum(ta881.id), sum(tb881.id) FROM ta881 JOIN tb881 ON ta881.n = tb881.n;
EXPLAIN SELECT ta881.id, tb881.id FROM ta881 RIGHT JOIN tb881 ON ta881.by = tb881.by WHERE tb881.id < 40;
SELECT ta881.id, tb881.id FROM ta881 RIGHT JOIN tb881 ON ta881.by = tb881.by WHERE tb881.id < 40;
RESET work_mem;
RESET TimeZone;
-- varchar and cidr compare as text and inet: the join's conditions print
-- their operands relabeled, and inet values are stored at their own width.
CREATE TABLE va881 (id int, vc varchar(10), c cidr);
CREATE TABLE vb881 (id int, vc varchar(10), c cidr);
INSERT INTO va881 SELECT g, 'v' || (g % 31), ('10.0.' || (g % 9) || '.0/24')::cidr FROM generate_series(1, 200) g;
INSERT INTO vb881 SELECT g, 'v' || (g % 43), ('10.0.' || (g % 7) || '.0/24')::cidr FROM generate_series(1, 120) g;
ANALYZE va881;
ANALYZE vb881;
EXPLAIN SELECT va881.id, vb881.id FROM va881 JOIN vb881 ON va881.vc = vb881.vc WHERE va881.id < 20;
SELECT va881.id, vb881.id FROM va881 JOIN vb881 ON va881.vc = vb881.vc WHERE va881.id < 20;
EXPLAIN SELECT count(*), sum(va881.id) FROM va881 JOIN vb881 ON va881.c = vb881.c;
SELECT count(*), sum(va881.id) FROM va881 JOIN vb881 ON va881.c = vb881.c;
EXPLAIN SELECT va881.id FROM va881 WHERE va881.vc IN (SELECT vc FROM vb881) AND va881.id < 30;
SELECT va881.id FROM va881 WHERE va881.vc IN (SELECT vc FROM vb881) AND va881.id < 30;
SET enable_hashjoin = off;
EXPLAIN SELECT va881.id, vb881.id FROM va881 JOIN vb881 ON va881.vc = vb881.vc WHERE vb881.id < 10;
SELECT va881.id, vb881.id FROM va881 JOIN vb881 ON va881.vc = vb881.vc WHERE vb881.id < 10;
RESET enable_hashjoin;
