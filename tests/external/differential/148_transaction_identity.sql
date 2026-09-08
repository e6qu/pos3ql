-- Full transaction identities, snapshots, status, arrays, and the historical
-- txid aliases retain PostgreSQL's SQL and catalog contracts.
CREATE TABLE diff_transaction_identity (
  id xid8 PRIMARY KEY,
  snap pg_snapshot NOT NULL,
  legacy txid_snapshot NOT NULL,
  ids xid8[] NOT NULL
);

BEGIN;
SELECT pg_current_xact_id_if_assigned() IS NULL,
       txid_current_if_assigned() IS NULL;
SELECT pg_current_xact_id() IS NOT NULL;
INSERT INTO diff_transaction_identity
SELECT pg_current_xact_id_if_assigned(), pg_current_snapshot(), txid_current_snapshot(),
       ARRAY[pg_current_xact_id_if_assigned(), '1'::xid8];
COMMIT;

SELECT pg_typeof(id), pg_typeof(snap), pg_typeof(legacy), pg_typeof(ids),
       pg_snapshot_xmin(snap) = id,
       pg_snapshot_xmax(snap) > id,
       pg_visible_in_snapshot(id, snap),
       txid_visible_in_snapshot(id::text::bigint, legacy),
       pg_xact_status(id), txid_status(id::text::bigint),
       age(id::xid) >= 0
  FROM diff_transaction_identity;

SELECT value, pg_typeof(value)
  FROM pg_snapshot_xip('10:20:11,14'::pg_snapshot) AS value;
SELECT txid_snapshot_xip('10:20:11,14'::txid_snapshot);
SELECT pg_snapshot_xmin('10:20:11,14'::pg_snapshot),
       pg_snapshot_xmax('10:20:11,14'::pg_snapshot),
       pg_visible_in_snapshot('12'::xid8, '10:20:11,14'::pg_snapshot),
       pg_visible_in_snapshot('14'::xid8, '10:20:11,14'::pg_snapshot);
SELECT '18446744073709551615'::xid8 > '9223372036854775807'::xid8,
       ('4294967297'::xid8)::xid::text,
       ARRAY['1'::xid8, '18446744073709551615'::xid8]::text;

CREATE SEQUENCE diff_transaction_identity_sequence;
BEGIN;
SELECT pg_current_xact_id_if_assigned() IS NULL;
SELECT nextval('diff_transaction_identity_sequence');
SELECT pg_current_xact_id_if_assigned() IS NOT NULL;
ROLLBACK;
BEGIN;
SELECT pg_current_xact_id_if_assigned() IS NULL;
SELECT nextval('diff_transaction_identity_sequence');
SELECT pg_current_xact_id_if_assigned() IS NULL;
ROLLBACK;
BEGIN;
SELECT setval('diff_transaction_identity_sequence', 20);
SELECT pg_current_xact_id_if_assigned() IS NOT NULL;
ROLLBACK;
DROP SEQUENCE diff_transaction_identity_sequence;

BEGIN;
SELECT pg_current_xact_id_if_assigned() IS NULL;
PREPARE TRANSACTION 'diff-transaction-identity-empty';
SELECT count(*), pg_xact_status(transaction::text::xid8)
  FROM pg_prepared_xacts WHERE gid = 'diff-transaction-identity-empty'
 GROUP BY transaction;
ROLLBACK PREPARED 'diff-transaction-identity-empty';

CREATE TABLE diff_transaction_identity_prepared_marker (id integer);
BEGIN;
SELECT pg_current_xact_id_if_assigned() IS NULL;
INSERT INTO diff_transaction_identity_prepared_marker VALUES (1);
SELECT pg_current_xact_id_if_assigned() IS NOT NULL;
PREPARE TRANSACTION 'diff-transaction-identity';
SELECT pg_current_xact_id() IS NOT NULL;
SELECT pg_xact_status(transaction::text::xid8),
       pg_snapshot_xip(pg_current_snapshot()) = transaction::text::xid8
  FROM pg_prepared_xacts WHERE gid = 'diff-transaction-identity';
ROLLBACK PREPARED 'diff-transaction-identity';
DROP TABLE diff_transaction_identity_prepared_marker;

SELECT oid, proname, prorettype, proretset, provolatile, proparallel,
       proargtypes::text
  FROM pg_proc
 WHERE oid IN (1181,2943,3348,2944,2945,2946,2947,2948,3360,
               5059,5060,5061,5062,5063,5064,5065,5066)
 ORDER BY oid;
SELECT oid, opcname, opcmethod, opcfamily, opcintype, opcdefault, opckeytype
  FROM pg_opclass WHERE oid = 10053;
SELECT oid, opfname, opfmethod, opfnamespace, opfowner
  FROM pg_opfamily WHERE oid = 5067;
SELECT oid, amopfamily, amoplefttype, amoprighttype, amopstrategy,
       amoppurpose, amopopr, amopmethod, amopsortfamily
  FROM pg_amop WHERE amopfamily = 5067 ORDER BY amopstrategy;
SELECT oid, amprocfamily, amproclefttype, amprocrighttype, amprocnum, amproc
  FROM pg_amproc WHERE amprocfamily = 5067 ORDER BY amprocnum;
SELECT oid, oprcanmerge, oprcanhash, oprleft, oprright, oprresult,
       oprcom, oprnegate, oprcode, oprrest, oprjoin
  FROM pg_operator
 WHERE oid IN (5068,5072,5073,5074,5075,5076)
 ORDER BY oid;

SELECT '10:9:'::pg_snapshot;
SELECT '10:20:14,13'::pg_snapshot;
SELECT '10:20:11,11,14'::pg_snapshot::text;
SELECT pg_xact_status('18446744073709551615'::xid8);

DROP TABLE diff_transaction_identity;
