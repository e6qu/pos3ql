-- PostgreSQL 18 tuple and command identity scalar/array, wire, index,
-- aggregate, support-routine, and catalog behavior.
CREATE TABLE low_level_identity_values (
  id integer PRIMARY KEY,
  tuple_id tid UNIQUE,
  command_id cid,
  tuple_ids tid[],
  command_ids cid[],
  default_tuple tid DEFAULT '(9,7)'::tid,
  default_command cid DEFAULT '42'::cid
);

INSERT INTO low_level_identity_values VALUES
  (1, '(0,1)', '0', ARRAY['(0,1)'::tid, NULL, '(4,2)'::tid],
   ARRAY['0'::cid, NULL, '4294967295'::cid], DEFAULT, DEFAULT),
  (2, '(4294967295,65535)', '4294967295', ARRAY[]::tid[], ARRAY[]::cid[],
   DEFAULT, DEFAULT),
  (3, NULL, NULL, NULL, NULL, DEFAULT, DEFAULT);

SELECT id, tuple_id, command_id, tuple_ids::text, command_ids::text,
       default_tuple, default_command, pg_typeof(tuple_id),
       pg_typeof(command_id), pg_typeof(tuple_ids), pg_typeof(command_ids)
FROM low_level_identity_values
ORDER BY tuple_id NULLS LAST;

SELECT '(0,0)'::tid, '(-1,65535)'::tid, 'prefix( 1, 2)trailing'::tid,
       '0'::cid, '-1'::cid, '+4294967295'::cid;

SELECT '(1,2)'::tid = '(1,2)'::tid,
       '(1,2)'::tid <> '(1,3)'::tid,
       '(1,2)'::tid < '(1,3)'::tid,
       '(1,2)'::tid <= '(1,2)'::tid,
       '(2,1)'::tid > '(1,65535)'::tid,
       '(2,1)'::tid >= '(2,1)'::tid,
       '42'::cid = '42'::cid;

SELECT tidin('(4,2)'), tidout('(4,2)'::tid), encode(tidsend('(4,2)'::tid), 'hex'),
       tideq('(4,2)', '(4,2)'), tidne('(4,2)', '(4,3)'),
       tidlt('(4,2)', '(4,3)'), tidle('(4,2)', '(4,2)'),
       tidgt('(4,3)', '(4,2)'), tidge('(4,2)', '(4,2)'),
       bttidcmp('(4,2)', '(4,3)'), tidlarger('(4,2)', '(4,3)'),
       tidsmaller('(4,2)', '(4,3)');

SELECT cidin('42'), cidout('42'::cid), encode(cidsend('42'::cid), 'hex'),
       cideq('42', '42');

SELECT hashtid('(0,0)'), hashtidextended('(0,0)', 0),
       hashtid('(4,2)'), hashtidextended('(4,2)', 123),
       hashcid('42'), hashcidextended('42', 0), hashcidextended('42', 123);

SELECT min(tuple_id), max(tuple_id) FROM low_level_identity_values;
SELECT id FROM low_level_identity_values
WHERE tuple_id >= '(0,1)'::tid ORDER BY tuple_id;
SELECT command_id, count(*) FROM low_level_identity_values
GROUP BY command_id ORDER BY count(*), command_id::text NULLS LAST;
SELECT ARRAY['1'::cid] = ARRAY['1'::cid],
       ARRAY['1'::cid] <> ARRAY['2'::cid];

SELECT oid, typname, typlen, typcategory, typarray, typelem,
       typinput, typoutput, typstorage
FROM pg_type WHERE oid IN (27,29,1010,1012) ORDER BY oid;

SELECT oid, proname, prorettype, proargtypes, provolatile, proisstrict,
       prokind, proleakproof
FROM pg_proc
WHERE oid IN (48,49,52,53,69,1265,1292,2233,2234,2438,2439,2442,2443,
              2790,2791,2792,2793,2794,2795,2796,2797,2798,6423,6424)
ORDER BY oid;

SELECT oid, oprname, oprleft, oprright, oprresult, oprcode, oprcom, oprnegate,
       oprcanmerge, oprcanhash
FROM pg_operator WHERE oid IN (385,387,402,2799,2800,2801,2802) ORDER BY oid;

SELECT oid, opfname, opfmethod
FROM pg_opfamily WHERE oid IN (2226,2227,2789) ORDER BY oid;
SELECT oid, opcname, opcfamily, opcintype, opcdefault
FROM pg_opclass WHERE oid IN (10050,10054,10055) ORDER BY oid;
SELECT oid, amopfamily, amopstrategy, amoppurpose, amopopr, amopmethod
FROM pg_amop WHERE oid IN (10055,10056,10057,10058,10059,10290,10291)
ORDER BY oid;
SELECT oid, amprocfamily, amproclefttype, amprocrighttype, amprocnum, amproc
FROM pg_amproc WHERE oid IN (10110,10111,10182,10183,10184,10185)
ORDER BY oid;
SELECT aggfnoid::oid, aggtransfn, aggcombinefn, aggsortop, aggtranstype
FROM pg_aggregate WHERE aggfnoid IN (2797,2798) ORDER BY aggfnoid;

COPY (SELECT tuple_id, command_id, tuple_ids, command_ids
      FROM low_level_identity_values ORDER BY id)
TO STDOUT (FORMAT text);

SELECT '1'::cid <> '2'::cid;
SELECT '1'::cid < '2'::cid;
SELECT min(command_id) FROM low_level_identity_values;
SELECT command_id FROM low_level_identity_values ORDER BY command_id;
SELECT ARRAY['1'::cid] < ARRAY['2'::cid];
SELECT '(1 ,2)'::tid;
SELECT '(1,2 )'::tid;
SELECT '(4294967296,1)'::tid;
SELECT '(1,65536)'::tid;
SELECT '4294967296'::cid;
SELECT '42x'::cid;
