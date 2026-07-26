-- Network address types (inet, cidr, macaddr, macaddr8), matching PostgreSQL 18.
-- Parsing/canonical output (including IPv6 :: compression and macaddr8 EUI-64
-- widening), ordering (v4 before v6, then address, then mask), casts, the
-- containment/bitwise/arithmetic operators, and the inspection functions.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS net_t;

CREATE TABLE net_t (id int, a inet, c cidr, m macaddr, m8 macaddr8);
INSERT INTO net_t VALUES
  (1, '10.0.0.1/8',    '192.168.100.0/24', '08:00:2b:01:02:03',       '08:00:2b:01:02:03:04:05'),
  (2, '2001:db8::1',   '10.0.0.0/8',        '08-00-2b-01-02-04',       '08:00:2b:01:02:03'),
  (3, '192.168.1.5/24','2001:db8::/32',     '0800.2b01.0205',          '01:02:03:04:05:06:07:08'),
  (4, '::ffff:1.2.3.4','::/0',              '08002b:010206',           '08:00:2b:ff:fe:01:02:03');

-- Canonical text output and ordering (network_cmp: family, address, mask).
SELECT id, a, c, m, m8 FROM net_t ORDER BY a;

-- IPv6 :: compression corner cases and default-mask omission.
SELECT '2001:0db8:0000:0000:0000:0000:0000:0001'::inet AS full,
       '1:0:0:1:0:0:0:1'::inet AS mid_run,
       '::1'::inet AS loopback,
       '::'::inet AS anyaddr,
       'fe80::1/64'::inet AS masked,
       '10.0.0.1'::inet AS v4_default;

-- cidr abbreviation and validation.
SELECT '10.1'::cidr AS abbreviated, abbrev('10.1.0.0/16'::cidr) AS ab, abbrev('192.168.1.5/24'::inet) AS ai;

-- Casts: inet <-> cidr (host bits dropped), macaddr <-> macaddr8.
SELECT '192.168.1.5/24'::inet::cidr AS to_cidr,
       '10.0.0.0/8'::cidr::inet AS to_inet,
       '08:00:2b:01:02:03'::macaddr::macaddr8 AS to_8,
       '08:00:2b:ff:fe:01:02:03'::macaddr8::macaddr AS to_6;

-- Containment / overlap / bitwise / arithmetic operators.
SELECT '192.168.1.5'::inet << '192.168.1.0/24'::inet AS sub,
       '192.168.1.5'::inet <<= '192.168.1.5/32'::inet AS subeq,
       '192.168.1.0/24'::inet >> '192.168.1.5'::inet AS sup,
       '192.168.1.0/24'::inet >>= '192.168.1.0/24'::inet AS supeq,
       '192.168.1.0/24'::cidr && '192.168.1.128/25'::cidr AS overlap,
       ~ '192.168.1.5'::inet AS notv,
       '192.168.1.5'::inet & '0.0.0.255'::inet AS andv,
       '192.168.1.0'::inet | '0.0.0.5'::inet AS orv,
       '192.168.1.5'::inet + 10 AS plus,
       '192.168.1.5'::inet - 10 AS minus,
       '192.168.1.20'::inet - '192.168.1.5'::inet AS diff;

-- Inspection functions.
SELECT family(a), host(a), masklen(a), broadcast(a), netmask(a), hostmask(a), network(a)
  FROM net_t WHERE id = 3;
SELECT set_masklen('192.168.1.5/24'::inet, 16) AS setlen,
       inet_same_family('1.2.3.4'::inet, '::1'::inet) AS samefam,
       inet_merge('192.168.1.5/24'::inet, '192.168.2.5/24'::inet) AS merged,
       trunc('08:00:2b:01:02:03'::macaddr) AS tmac,
       trunc('08:00:2b:01:02:03:04:05'::macaddr8) AS tmac8,
       macaddr8_set7bit('00:00:2b:01:02:03:04:05'::macaddr8) AS set7;

-- Type identity through pg_typeof, and DISTINCT/GROUP BY over network values.
SELECT pg_typeof(a), pg_typeof(c), pg_typeof(m), pg_typeof(m8) FROM net_t WHERE id = 1;
SELECT count(DISTINCT a) FROM net_t;
SELECT m, count(*) FROM net_t GROUP BY m ORDER BY m;

-- WHERE filtering on a network predicate.
SELECT id FROM net_t WHERE a << '10.0.0.0/8'::inet ORDER BY id;

-- Constant column defaults of each network type.
DROP TABLE IF EXISTS net_def;
CREATE TABLE net_def (
  id int,
  a inet    DEFAULT '0.0.0.0',
  c cidr    DEFAULT '10.0.0.0/8',
  m macaddr DEFAULT '00:00:00:00:00:00'
);
INSERT INTO net_def(id) VALUES (1);
SELECT a, c, m FROM net_def;
DROP TABLE net_def;

-- Errors: invalid literals (22P02), cidr host bits (22P02).
SELECT 'zzz'::inet;
SELECT '256.1.1.1'::inet;
SELECT '192.168.1.5/24'::cidr;
SELECT 'gg:00:2b:01:02:03'::macaddr;

DROP TABLE net_t;
