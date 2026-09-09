-- Complete the executable PostgreSQL 18 network-address scalar boundary:
-- support routines, exact ordering, hashes, binary sends, MAC bitwise
-- operators, extrema, catalogs, indexes, constraints, generated columns,
-- views, and updates.
DROP VIEW IF EXISTS network_complete_view;
DROP TABLE IF EXISTS network_complete;

SELECT inet_out(inet_in('192.168.1.5/24')),
       cidr_out(cidr_in('192.168.1.0/24')),
       macaddr_out(macaddr_in('08-00-2b-01-02-03')),
       macaddr8_out(macaddr8_in('08:00:2b:01:02:03'));
SELECT text('192.168.1.5/24'::inet),
       text('2001:db8::1/64'::inet),
       '192.168.1.0/24'::cidr::varchar,
       '192.168.1.5/24'::inet::character(20),
       cidr('192.168.1.5/24'::inet),
       macaddr8('08:00:2b:01:02:03'::macaddr),
       macaddr('08:00:2b:ff:fe:01:02:03'::macaddr8),
       pg_typeof(set_masklen('192.168.1.0/24'::cidr, 16));

SELECT encode(inet_send('192.168.1.5/24'::inet), 'hex'),
       encode(cidr_send('192.168.1.0/24'::cidr), 'hex'),
       encode(inet_send('2001:db8::1/64'::inet), 'hex'),
       encode(macaddr_send('08:00:2b:01:02:03'::macaddr), 'hex'),
       encode(macaddr8_send('08:00:2b:01:02:03:04:05'::macaddr8), 'hex');

-- Network comparison considers shared prefix, then mask width, then host bits.
SELECT network_cmp('10.0.0.1/8', '10.0.0.0/24'),
       network_cmp('10.0.0.0/8', '10.0.0.0/24'),
       network_cmp('10.0.0.1/24', '10.0.0.2/8'),
       network_cmp('10.0.0.0/24', '10.0.0.0/25'),
       network_cmp('::1', '0.0.0.0');
SELECT network_eq('10.0.0.1/8', '10.0.0.1/8'),
       network_ne('10.0.0.1/8', '10.0.0.0/8'),
       network_lt('10.0.0.1/8', '10.0.0.0/24'),
       network_le('10.0.0.1/8', '10.0.0.0/24'),
       network_gt('10.0.0.0/24', '10.0.0.1/8'),
       network_ge('10.0.0.0/24', '10.0.0.1/8');

SELECT network_sub('192.168.1.5', '192.168.1.0/24'),
       network_subeq('192.168.1.5', '192.168.1.5/32'),
       network_sup('192.168.1.0/24', '192.168.1.5'),
       network_supeq('192.168.1.0/24', '192.168.1.0/24'),
       network_overlap('192.168.1.0/24', '192.168.1.128/25');
SELECT network_larger('10.0.0.1/8', '10.0.0.0/24'),
       network_smaller('10.0.0.1/8', '10.0.0.0/24'),
       inetnot('192.168.1.5'),
       inetand('192.168.1.5', '0.0.0.255'),
       inetor('192.168.1.0', '0.0.0.5'),
       inetpl('192.168.1.5', 10),
       int8pl_inet(10, '192.168.1.5'),
       inetmi_int8('192.168.1.5', 10),
       inetmi('192.168.1.20', '192.168.1.5');
SELECT 'ffff::1'::inet + (-9223372036854775807::bigint - 1),
       inetpl('ffff::1', (-9223372036854775807::bigint - 1));
SELECT '::1'::inet - (-9223372036854775807::bigint - 1);

SELECT hashinet('192.168.1.5/24'),
       hashinetextended('192.168.1.5/24', 123),
       hashmacaddr('08:00:2b:01:02:03'),
       hashmacaddrextended('08:00:2b:01:02:03', 123),
       hashmacaddr8('08:00:2b:01:02:03:04:05'),
       hashmacaddr8extended('08:00:2b:01:02:03:04:05', 123);

SELECT macaddr_eq('08:00:2b:01:02:03', '08:00:2b:01:02:03'),
       macaddr_lt('08:00:2b:01:02:03', '08:00:2b:01:02:04'),
       macaddr_cmp('08:00:2b:01:02:04', '08:00:2b:01:02:03'),
       macaddr_not('08:00:2b:01:02:03'),
       macaddr_and('08:00:2b:01:02:03', 'ff:00:ff:00:ff:00'),
       macaddr_or('08:00:2b:01:02:03', '00:ff:00:ff:00:ff');
SELECT macaddr8_eq('08:00:2b:01:02:03:04:05', '08:00:2b:01:02:03:04:05'),
       macaddr8_gt('08:00:2b:01:02:03:04:06', '08:00:2b:01:02:03:04:05'),
       macaddr8_cmp('08:00:2b:01:02:03:04:05', '08:00:2b:01:02:03:04:06'),
       macaddr8_not('08:00:2b:01:02:03:04:05'),
       macaddr8_and('08:00:2b:01:02:03:04:05', 'ff:00:ff:00:ff:00:ff:00'),
       macaddr8_or('08:00:2b:01:02:03:04:05', '00:ff:00:ff:00:ff:00:ff');
SELECT ~'08:00:2b:01:02:03'::macaddr,
       '08:00:2b:01:02:03'::macaddr & 'ff:00:ff:00:ff:00'::macaddr,
       '08:00:2b:01:02:03'::macaddr | '00:ff:00:ff:00:ff'::macaddr,
       ~'08:00:2b:01:02:03:04:05'::macaddr8,
       '08:00:2b:01:02:03:04:05'::macaddr8 & 'ff:00:ff:00:ff:00:ff:00'::macaddr8;

CREATE TABLE network_complete (
  id integer PRIMARY KEY,
  address inet NOT NULL UNIQUE,
  subnet cidr NOT NULL,
  hardware macaddr NOT NULL,
  hardware8 macaddr8 NOT NULL,
  masked inet GENERATED ALWAYS AS (address & '255.255.255.0'::inet) STORED,
  CHECK (address <<= subnet)
);
CREATE INDEX network_complete_subnet_idx ON network_complete (subnet);
CREATE INDEX network_complete_hardware_idx ON network_complete (hardware);
CREATE INDEX network_complete_hardware8_idx ON network_complete (hardware8);
INSERT INTO network_complete (id, address, subnet, hardware, hardware8) VALUES
  (1, '10.0.0.1/8', '10.0.0.0/8', '08:00:2b:01:02:03', '08:00:2b:01:02:03:04:05'),
  (2, '10.0.0.0/24', '10.0.0.0/8', '08:00:2b:01:02:04', '08:00:2b:01:02:03:04:06'),
  (3, '10.0.0.0/25', '10.0.0.0/8', '08:00:2b:01:02:05', '08:00:2b:01:02:03:04:07');
CREATE VIEW network_complete_view AS
  SELECT min(address) AS first_address, max(address) AS last_address,
         count(DISTINCT hardware) AS hardware_count
    FROM network_complete;
SELECT id, address, masked FROM network_complete ORDER BY address;
SELECT * FROM network_complete_view;
SELECT id FROM network_complete WHERE subnet = '10.0.0.0/8'::cidr ORDER BY id;
SELECT id FROM network_complete WHERE hardware >= '08:00:2b:01:02:04'::macaddr ORDER BY id;
UPDATE network_complete
   SET address = inetpl(address, 16), hardware = macaddr_or(hardware, '00:00:00:00:00:10')
 WHERE id = 1;
SELECT id, address, hardware, masked FROM network_complete ORDER BY id;

-- PostgreSQL's built-in identities and index contracts are client-visible.
SELECT oid, proname, prorettype, proargtypes::text, prokind, provolatile, proisstrict
  FROM pg_proc
 WHERE oid IN (422, 778, 779, 781, 836, 926, 2495, 2497, 2499, 2633, 3359,
               3447, 3562, 3563, 3564, 3565, 4119, 5033, 5051)
 ORDER BY oid;
SELECT oid, oprname, oprkind, oprleft, oprright, oprresult, oprcom, oprnegate,
       oprcanmerge, oprcanhash, oprcode::text
  FROM pg_operator
 WHERE oid IN (931, 1201, 1203, 1220, 2634, 2637, 2640, 3147, 3362, 3368, 3552)
 ORDER BY oid;
SELECT oid, castsource, casttarget, castfunc, castcontext, castmethod
  FROM pg_cast
 WHERE oid IN (10185,10186,10187,10188,10195,10196,10200,10201,10205,10206)
 ORDER BY oid;
SELECT oid, opfmethod, opfname FROM pg_opfamily
 WHERE oid IN (1974,1975,1984,1985,3371,3372) ORDER BY oid;
SELECT oid, opcmethod, opcname, opcfamily, opcintype, opcdefault FROM pg_opclass
 WHERE oid IN (10009,10010,10015,10016,10024,10025,10026,10027) ORDER BY oid;
SELECT amopfamily, amopstrategy, amopopr, amopmethod FROM pg_amop
 WHERE amopfamily IN (1974,1975,1984,1985,3371,3372)
 ORDER BY amopfamily, amopstrategy;
SELECT amprocfamily, amprocnum, amproc::text FROM pg_amproc
 WHERE amprocfamily IN (1974,1975,1984,1985,3371,3372)
 ORDER BY amprocfamily, amprocnum;
SELECT aggfnoid::oid, aggtransfn::text, aggcombinefn::text, aggsortop,
       aggtranstype FROM pg_aggregate
 WHERE aggfnoid::oid IN (3564,3565) ORDER BY aggfnoid::oid;

DROP VIEW network_complete_view;
DROP TABLE network_complete;
