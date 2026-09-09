-- PostgreSQL 18 binary strings: byte addressing, slicing, pattern operations,
-- checksums, integer casts, aggregation, catalogs, and durable expressions.
SELECT encode(substring('\x0011223344'::bytea FROM -1 FOR 4), 'hex'),
       encode(substring('\x0011223344'::bytea FROM 0 FOR 4), 'hex'),
       encode(substr('\x0011223344'::bytea, 2, 2), 'hex'),
       encode(substring('\x0011223344'::bytea FROM 4), 'hex');
SELECT encode(overlay('\x0011223344'::bytea PLACING '\xaabb'::bytea FROM 2), 'hex'),
       encode(overlay('\x0011223344'::bytea PLACING '\xaabb'::bytea FROM 2 FOR 1), 'hex'),
       position('\x1122'::bytea IN '\x0011223344'::bytea),
       position(''::bytea IN '\x0011'::bytea);
SELECT encode(btrim('\x002000'::bytea, '\x00'::bytea), 'hex'),
       encode(ltrim('\x00010200'::bytea, '\x0001'::bytea), 'hex'),
       encode(rtrim('\x00010200'::bytea, '\x0002'::bytea), 'hex'),
       encode(reverse('\x001122'::bytea), 'hex');
SELECT encode(reverse(substring(reverse('\x00112233'::bytea) FROM 2 FOR 2)), 'hex'),
       encode(overlay(reverse('\x001122'::bytea) PLACING '\xff'::bytea FROM 2), 'hex');
SELECT encode('\x0061'::bytea || '\x6200'::bytea, 'hex'),
       pg_typeof('\x00'::bytea || '\x01'::bytea),
       '\x006162ff'::bytea LIKE '\x0025ff'::bytea,
       '\x006162ff'::bytea NOT LIKE '\x005fff'::bytea;
SELECT '\x612562'::bytea LIKE '\x61212562'::bytea ESCAPE '\x21'::bytea;
SELECT md5('\x00ff'::bytea), crc32('\x313233343536373839'::bytea),
       crc32c('\x313233343536373839'::bytea);
SELECT byteacmp('\x00'::bytea, '\xff'::bytea),
       encode(byteacat('\x00'::bytea, '\xff'::bytea), 'hex'),
       bytealike('\x612562'::bytea, '\x612562'::bytea),
       hashbytea(''::bytea),
       hashbytea('\x000102030405060708090a0b0c0d'::bytea),
       hashbyteaextended('\x000102030405060708090a0b0c0d'::bytea, 123);
SELECT get_byte('\x00ff'::bytea, 1), encode(set_byte('\x00'::bytea, 0, 257), 'hex'),
       get_bit('\x02'::bytea, 1), encode(set_bit('\x00'::bytea, 3, 1), 'hex');

-- PostgreSQL 18 integer casts use fixed-width network byte order and accept a
-- shorter bytea by zero extension.
SELECT encode((-1::smallint)::bytea, 'hex'), encode((258::smallint)::bytea, 'hex'),
       encode((16909060::integer)::bytea, 'hex'),
       encode((72623859790382856::bigint)::bytea, 'hex');
SELECT '\xff'::bytea::smallint, '\xffff'::bytea::smallint,
       '\xffffffff'::bytea::integer, '\xffffffffffffffff'::bytea::bigint,
       ''::bytea::integer;

-- Bit-string scalar functions operate at the most-significant-bit boundary
-- and, like PostgreSQL, return the fixed bit type.
SELECT substring(B'001101' FROM -1 FOR 4), substring(B'001101' FROM 2 FOR 3),
       overlay(B'001101' PLACING B'11' FROM 2 FOR 3), position(B'11' IN B'001101');
SELECT get_bit(B'1001', 0), get_bit(B'1001', 3), set_bit(B'1001', 1, 1),
       pg_typeof(substring(B'001101'::varbit FROM 2)),
       pg_typeof(set_bit(B'1001'::varbit, 1, 1));
SELECT bitcmp(B'10', B'01'), bitand(B'10', B'11'), bitnot(B'10'),
       bitshiftleft(B'101', 1),
       encode(bit_send(B'100000001'), 'hex'),
       encode(varbit_send(B'100000001'::varbit), 'hex');
SELECT bit_and(value), bit_or(value), bit_xor(value), pg_typeof(bit_and(value))
FROM (VALUES (B'1100'::varbit), (B'1010'::varbit), (NULL::varbit)) AS inputs(value);

-- The bytea aggregate retains raw zero/non-UTF-8 bytes and each ordered row's
-- own delimiter. DISTINCT applies to the complete (value, delimiter) pair.
SELECT encode(string_agg(value, delimiter ORDER BY ordinal), 'hex')
FROM (VALUES (2, '\x62'::bytea, '\x2d'::bytea),
             (1, '\x6100'::bytea, '\x2f'::bytea),
             (3, NULL::bytea, '\x00'::bytea)) AS inputs(ordinal, value, delimiter);
SELECT encode(string_agg(DISTINCT value, delimiter), 'hex')
FROM (VALUES ('\x61'::bytea, '\x2d'::bytea),
             ('\x61'::bytea, '\x2f'::bytea),
             ('\x61'::bytea, '\x2d'::bytea)) AS inputs(value, delimiter);
SELECT string_agg(value, delimiter ORDER BY ordinal)
FROM (VALUES (2, 'b', '-'), (1, 'a', '/')) AS inputs(ordinal, value, delimiter);
SELECT string_agg(value, delimiter ORDER BY ordinal)
FROM (VALUES (1, '', ','), (2, 'a', ';')) AS inputs(ordinal, value, delimiter);
SELECT encode(string_agg(value, delimiter ORDER BY ordinal), 'hex')
FROM (VALUES (1, ''::bytea, '\x2d'::bytea),
             (2, '\x61'::bytea, '\x2f'::bytea)) AS inputs(ordinal, value, delimiter);
SELECT min(value), max(value)
FROM (VALUES ('\xff'::bytea), ('\x00ff'::bytea), ('\x0100'::bytea)) AS inputs(value);

CREATE TABLE binary_values (
  id integer PRIMARY KEY,
  payload bytea NOT NULL,
  flags bit(8) NOT NULL,
  tail bytea GENERATED ALWAYS AS (substring(payload FROM 2)) STORED,
  CHECK (crc32(payload) >= 0)
);
CREATE INDEX binary_values_payload_idx ON binary_values (payload);
INSERT INTO binary_values (id, payload, flags) VALUES
  (1, '\x001122'::bytea, B'10101010'),
  (2, '\xff00'::bytea, B'00001111');
SELECT id, encode(payload, 'hex'), flags, encode(tail, 'hex')
FROM binary_values ORDER BY payload;
UPDATE binary_values SET payload = set_byte(payload, 0, 1), flags = set_bit(flags, 0, 1)
WHERE id = 1 RETURNING id, encode(payload, 'hex'), flags, encode(tail, 'hex');
SELECT payload, count(*) FROM binary_values GROUP BY payload ORDER BY payload;

-- Exact catalog identities for the newly closed scalar, aggregate, operator,
-- and explicit-cast surfaces.
SELECT oid, proname, prorettype, proargtypes::text, provolatile, proisstrict
FROM pg_proc
WHERE oid IN (720, 721, 722, 723, 724, 749, 752, 1680, 1698, 1699, 1810,
              1946, 1947, 2009, 2010, 2012, 2013, 2014, 2015, 2085, 2086,
              2321, 3030, 3031, 3032, 3033, 3331, 3419, 3420, 3421, 3422,
              3545, 6162, 6163, 6167, 6364, 6365, 6367, 6368, 6369,
              6370, 6371, 6372, 6382, 6395, 6396, 6413, 6414)
ORDER BY oid;
SELECT oid, oprname, oprleft, oprright, oprresult, oprcom, oprnegate,
       oprcanmerge, oprcanhash, oprcode::oid
FROM pg_operator
WHERE oid IN (1784, 1785, 1786, 1787, 1788, 1789, 1791, 1792, 1793,
              1795, 1796, 1797, 1804, 1805, 1806, 1807, 1808, 1809,
              1955, 1956, 1957, 1958, 1959, 1960, 2016, 2017, 2018)
ORDER BY oid;
SELECT oid, castsource, casttarget, castfunc, castcontext, castmethod
FROM pg_cast WHERE oid BETWEEN 10143 AND 10148 ORDER BY oid;
SELECT aggfnoid::oid, aggtransfn::oid, aggfinalfn::oid, aggcombinefn::oid,
       aggsortop::oid, aggtranstype::oid
FROM pg_aggregate
WHERE aggfnoid IN (2242, 2243, 3545, 6167, 6395, 6396)
ORDER BY aggfnoid;
SELECT oid, opfmethod, opfname FROM pg_opfamily
WHERE oid IN (423, 428, 2002, 2223) ORDER BY oid;
SELECT oid, opcmethod, opcname, opcfamily, opcintype, opcdefault
FROM pg_opclass WHERE oid IN (10002, 10006, 10043, 10049) ORDER BY oid;
SELECT oid, amopfamily, amoplefttype, amopstrategy, amopopr, amopmethod
FROM pg_amop WHERE amopfamily IN (423, 428, 2002, 2223)
ORDER BY amopfamily, amopstrategy;
SELECT oid, amprocfamily, amproclefttype, amprocnum, amproc::oid
FROM pg_amproc WHERE amprocfamily IN (423, 428, 2002, 2223)
ORDER BY amprocfamily, amprocnum;

-- Error boundaries: no wrapping bit values, no fabricated out-of-range slice,
-- and no too-wide integer conversion.
SELECT set_bit('\x00'::bytea, 0, 2);
SELECT set_bit(B'0', 0, -1);
SELECT get_bit(''::bytea, 0);
SELECT substring('\x00'::bytea FROM 1 FOR -1);
SELECT '\x010203'::bytea::smallint;
SELECT 'a' LIKE E'\\';
SELECT '\x61'::bytea LIKE '\x5c'::bytea;

DROP TABLE binary_values;
