-- PostgreSQL 18 aclitem, pg_lsn, object-privilege overload, support-routine,
-- and catalog behavior.

CREATE ROLE aclitem_reader;
CREATE ROLE aclitem_grantor;
CREATE ROLE aclitem_object_user;

CREATE TABLE aclitem_surface (id integer PRIMARY KEY, visible text);
GRANT SELECT ON aclitem_surface TO aclitem_reader;
GRANT UPDATE (visible) ON aclitem_surface TO aclitem_reader;

SELECT 'aclitem_reader=w*r/aclitem_grantor'::aclitem,
       aclitemout(aclitemin('aclitem_reader=wr*/aclitem_grantor'));
SELECT aclitemeq('aclitem_reader=rw*/aclitem_grantor',
                 'aclitem_reader=w*r/aclitem_grantor'),
       aclcontains(ARRAY['aclitem_reader=rw/aclitem_grantor'::aclitem],
                   'aclitem_reader=r/aclitem_grantor');
SELECT makeaclitem('aclitem_reader'::regrole, 'aclitem_grantor'::regrole,
                   'SELECT', true);
SELECT hash_aclitem(makeaclitem(10, 10, 'SELECT', true)),
       hash_aclitem_extended(makeaclitem(10, 10, 'SELECT', true), 0),
       hash_aclitem_extended(makeaclitem(10, 10, 'SELECT', true), 123);
SELECT grantor::regrole, grantee::regrole, privilege_type, is_grantable
FROM aclexplode(ARRAY['aclitem_reader=rw*/aclitem_grantor'::aclitem,
                      '=U/aclitem_grantor'::aclitem]);

SELECT pg_get_acl('pg_class'::regclass, 'aclitem_surface'::regclass, 0)::text;
SELECT pg_get_acl('pg_class'::regclass, 'aclitem_surface'::regclass, 2)::text;

CREATE FOREIGN DATA WRAPPER aclitem_fdw NO HANDLER NO VALIDATOR;
CREATE SERVER aclitem_server FOREIGN DATA WRAPPER aclitem_fdw;
GRANT USAGE ON FOREIGN DATA WRAPPER aclitem_fdw TO aclitem_object_user;
GRANT USAGE ON FOREIGN SERVER aclitem_server TO aclitem_object_user;
SELECT has_foreign_data_wrapper_privilege(
         'aclitem_object_user', 'aclitem_fdw', 'USAGE'),
       has_foreign_data_wrapper_privilege(
         'aclitem_object_user'::regrole,
         (SELECT oid FROM pg_foreign_data_wrapper WHERE fdwname = 'aclitem_fdw'),
         'USAGE'),
       has_server_privilege('aclitem_object_user', 'aclitem_server', 'USAGE'),
       has_server_privilege(
         'aclitem_object_user'::regrole,
         (SELECT oid FROM pg_foreign_server WHERE srvname = 'aclitem_server'),
         'USAGE');

SELECT lo_create(92831::oid);
GRANT SELECT ON LARGE OBJECT 92831 TO aclitem_object_user;
SELECT has_largeobject_privilege('aclitem_object_user', 92831::oid, 'SELECT'),
       has_largeobject_privilege(92831::oid, 'UPDATE');

SELECT '0/10'::pg_lsn, 'abcdef01/23456789'::pg_lsn,
       pg_lsn(4294967297::numeric);
SELECT '0/10'::pg_lsn = '0/10'::pg_lsn,
       '0/10'::pg_lsn <> '0/11'::pg_lsn,
       '0/10'::pg_lsn < '1/0'::pg_lsn,
       '0/10'::pg_lsn <= '0/10'::pg_lsn,
       '1/0'::pg_lsn > '0/FFFFFFFF'::pg_lsn,
       '1/0'::pg_lsn >= '1/0'::pg_lsn;
SELECT '0/10'::pg_lsn + 1.5::numeric,
       1.5::numeric + '0/10'::pg_lsn,
       '0/10'::pg_lsn - 1.5::numeric,
       '1/0'::pg_lsn - '0/FFFFFFFF'::pg_lsn,
       pg_wal_lsn_diff('1/0', '0/FFFFFFFF');
SELECT pg_lsn_cmp('0/10', '0/20'), pg_lsn_larger('0/10', '0/20'),
       pg_lsn_smaller('0/10', '0/20'), pg_lsn_hash('0/1'),
       pg_lsn_hash_extended('FFFFFFFF/FFFFFFFF', 123);

CREATE TABLE aclitem_lsn_values (
  id integer PRIMARY KEY,
  position pg_lsn UNIQUE,
  positions pg_lsn[],
  access aclitem[]
);
INSERT INTO aclitem_lsn_values VALUES
  (1, '1/10', ARRAY['1/20'::pg_lsn, 'FFFFFFFF/FFFFFFFF'::pg_lsn],
   ARRAY['aclitem_reader=r/aclitem_grantor'::aclitem]),
  (2, '0/FF', ARRAY[]::pg_lsn[], ARRAY[]::aclitem[]),
  (3, NULL, NULL, NULL);
SELECT id, position, positions::text, access::text,
       pg_typeof(position), pg_typeof(positions), pg_typeof(access)
FROM aclitem_lsn_values ORDER BY position NULLS LAST;
SELECT min(position), max(position) FROM aclitem_lsn_values;

CREATE ROLE aclitem_old_identity;
CREATE ROLE aclitem_old_grantor;
CREATE TABLE aclitem_identity_values (
  direct aclitem,
  members aclitem[],
  defaulted aclitem DEFAULT 'aclitem_old_identity=w/aclitem_old_grantor'::aclitem
);
INSERT INTO aclitem_identity_values (direct, members)
VALUES ('aclitem_old_identity=r*/aclitem_old_grantor'::aclitem,
        ARRAY['aclitem_old_identity=r/aclitem_old_grantor'::aclitem]);
ALTER ROLE aclitem_old_identity RENAME TO aclitem_new_identity;
ALTER ROLE aclitem_old_grantor RENAME TO aclitem_new_grantor;
SELECT direct::text, members::text, defaulted::text,
       members @> 'aclitem_new_identity=r/aclitem_new_grantor'::aclitem
FROM aclitem_identity_values;
DROP ROLE aclitem_new_identity;
DROP ROLE aclitem_new_grantor;
SELECT direct::text ~ '^[0-9]+=r\*/[0-9]+$',
       members::text ~ '^\{[0-9]+=r/[0-9]+\}$',
       defaulted::text ~ '^[0-9]+=w/[0-9]+$'
FROM aclitem_identity_values;

SELECT oid, typname, typlen, typcategory, typarray, typelem,
       typinput, typoutput, typstorage
FROM pg_type WHERE oid IN (1033,1034,3220,3221) ORDER BY oid;

SELECT oid, proname, prorettype, proargtypes, pronargs, proretset,
       provolatile, proisstrict, proparallel, prosrc
FROM pg_proc
WHERE oid IN (329,777,1031,1032,1035,1036,1037,1062,1365,1689,3000,3001,
              3002,3003,3004,3005,3006,3007,3008,3009,3010,3011,3165,3229,
              3230,3231,3232,3233,3234,3235,3236,3237,3238,3239,3251,3252,
              3413,3943,4187,4188,4189,4190,5022,5023,5024,6103,6348,6349,
              6350,6385)
ORDER BY oid;
SELECT oid, oprname, oprleft, oprright, oprresult, oprcode, oprcom, oprnegate,
       oprcanmerge, oprcanhash
FROM pg_operator
WHERE oid IN (966,967,968,974,3222,3223,3224,3225,3226,3227,3228,5025,
              5026,5027)
ORDER BY oid;
SELECT oid, opfname, opfmethod
FROM pg_opfamily WHERE oid IN (2235,3253,3254) ORDER BY oid;
SELECT oid, opcname, opcfamily, opcintype, opcdefault
FROM pg_opclass WHERE oid IN (10059,10067,10068) ORDER BY oid;
SELECT oid, amopfamily, amopstrategy, amoppurpose, amopopr, amopmethod
FROM pg_amop WHERE oid IN (10250,10251,10252,10253,10254,10294,10296)
ORDER BY oid;
SELECT oid, amprocfamily, amproclefttype, amprocrighttype, amprocnum, amproc
FROM pg_amproc WHERE oid IN (10118,10119,10190,10191,10196,10197)
ORDER BY oid;
SELECT aggfnoid::oid, aggtransfn, aggcombinefn, aggsortop, aggtranstype
FROM pg_aggregate WHERE aggfnoid IN (4189,4190) ORDER BY aggfnoid;

COPY (SELECT position, positions, access FROM aclitem_lsn_values ORDER BY id)
TO STDOUT (FORMAT text);

SELECT ARRAY['aclitem_reader=r/aclitem_grantor'::aclitem, NULL] @>
       'aclitem_reader=r/aclitem_grantor'::aclitem;
SELECT ARRAY['aclitem_reader=r/aclitem_grantor'::aclitem] +
       'aclitem_reader=w/aclitem_grantor'::aclitem;
SELECT 'missing_acl_role=r/aclitem_grantor'::aclitem;
SELECT 'FFFFFFFF/FFFFFFFF'::pg_lsn + 1;
SELECT 'not-an-lsn'::pg_lsn;

SELECT lo_unlink(92831::oid);
DROP TABLE aclitem_identity_values;
DROP TABLE aclitem_lsn_values;
DROP TABLE aclitem_surface;
DROP SERVER aclitem_server;
DROP FOREIGN DATA WRAPPER aclitem_fdw;
DROP ROLE aclitem_object_user;
DROP ROLE aclitem_reader;
DROP ROLE aclitem_grantor;
