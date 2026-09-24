-- PostgreSQL 18 is the oracle for complete bounded callable and policy shapes.
CREATE FUNCTION differential_wide_arguments(
  a0 integer DEFAULT 1000000,
  a1 integer DEFAULT 1000000,
  a2 integer DEFAULT 1000000,
  a3 integer DEFAULT 1000000,
  a4 integer DEFAULT 1000000,
  a5 integer DEFAULT 1000000,
  a6 integer DEFAULT 1000000,
  a7 integer DEFAULT 1000000,
  a8 integer DEFAULT 1000000,
  a9 integer DEFAULT 1000000,
  a10 integer DEFAULT 1000000,
  a11 integer DEFAULT 1000000,
  a12 integer DEFAULT 1000000,
  a13 integer DEFAULT 1000000,
  a14 integer DEFAULT 1000000,
  a15 integer DEFAULT 1000000,
  a16 integer DEFAULT 1000000,
  a17 integer DEFAULT 1000000,
  a18 integer DEFAULT 1000000,
  a19 integer DEFAULT 1000000,
  a20 integer DEFAULT 1000000,
  a21 integer DEFAULT 1000000,
  a22 integer DEFAULT 1000000,
  a23 integer DEFAULT 1000000,
  a24 integer DEFAULT 1000000,
  a25 integer DEFAULT 1000000,
  a26 integer DEFAULT 1000000,
  a27 integer DEFAULT 1000000,
  a28 integer DEFAULT 1000000,
  a29 integer DEFAULT 1000000,
  a30 integer DEFAULT 1000000,
  a31 integer DEFAULT 1000000,
  a32 integer DEFAULT 1000000,
  a33 integer DEFAULT 1000000,
  a34 integer DEFAULT 1000000,
  a35 integer DEFAULT 1000000,
  a36 integer DEFAULT 1000000,
  a37 integer DEFAULT 1000000,
  a38 integer DEFAULT 1000000,
  a39 integer DEFAULT 1000000,
  a40 integer DEFAULT 1000000,
  a41 integer DEFAULT 1000000,
  a42 integer DEFAULT 1000000,
  a43 integer DEFAULT 1000000,
  a44 integer DEFAULT 1000000,
  a45 integer DEFAULT 1000000,
  a46 integer DEFAULT 1000000,
  a47 integer DEFAULT 1000000,
  a48 integer DEFAULT 1000000,
  a49 integer DEFAULT 1000000,
  a50 integer DEFAULT 1000000,
  a51 integer DEFAULT 1000000,
  a52 integer DEFAULT 1000000,
  a53 integer DEFAULT 1000000,
  a54 integer DEFAULT 1000000,
  a55 integer DEFAULT 1000000,
  a56 integer DEFAULT 1000000,
  a57 integer DEFAULT 1000000,
  a58 integer DEFAULT 1000000,
  a59 integer DEFAULT 1000000,
  a60 integer DEFAULT 1000000,
  a61 integer DEFAULT 1000000,
  a62 integer DEFAULT 1000000,
  a63 integer DEFAULT 1000000,
  a64 integer DEFAULT 1000000,
  a65 integer DEFAULT 1000000,
  a66 integer DEFAULT 1000000,
  a67 integer DEFAULT 1000000,
  a68 integer DEFAULT 1000000,
  a69 integer DEFAULT 1000000,
  a70 integer DEFAULT 1000000,
  a71 integer DEFAULT 1000000,
  a72 integer DEFAULT 1000000,
  a73 integer DEFAULT 1000000,
  a74 integer DEFAULT 1000000,
  a75 integer DEFAULT 1000000,
  a76 integer DEFAULT 1000000,
  a77 integer DEFAULT 1000000,
  a78 integer DEFAULT 1000000,
  a79 integer DEFAULT 1000000,
  a80 integer DEFAULT 1000000,
  a81 integer DEFAULT 1000000,
  a82 integer DEFAULT 1000000,
  a83 integer DEFAULT 1000000,
  a84 integer DEFAULT 1000000,
  a85 integer DEFAULT 1000000,
  a86 integer DEFAULT 1000000,
  a87 integer DEFAULT 1000000,
  a88 integer DEFAULT 1000000,
  a89 integer DEFAULT 1000000,
  a90 integer DEFAULT 1000000,
  a91 integer DEFAULT 1000000,
  a92 integer DEFAULT 1000000,
  a93 integer DEFAULT 1000000,
  a94 integer DEFAULT 1000000,
  a95 integer DEFAULT 1000000,
  a96 integer DEFAULT 1000000,
  a97 integer DEFAULT 1000000,
  a98 integer DEFAULT 1000000,
  a99 integer DEFAULT 1000000
) RETURNS integer LANGUAGE SQL AS 'SELECT $1 + $100';
SELECT differential_wide_arguments();
SELECT differential_wide_arguments(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99);
SELECT pronargs, pronargdefaults FROM pg_proc WHERE proname = 'differential_wide_arguments';
SELECT length(proargdefaults::text) > 256 FROM pg_proc WHERE proname = 'differential_wide_arguments';
SELECT length('differential_wide_arguments(integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer,integer)'::regprocedure::text) > 256;
CREATE FUNCTION differential_wide_result() RETURNS TABLE (f0 integer, f1 integer, f2 integer, f3 integer, f4 integer, f5 integer, f6 integer, f7 integer, f8 integer, f9 integer, f10 integer, f11 integer, f12 integer, f13 integer, f14 integer, f15 integer, f16 integer, f17 integer, f18 integer, f19 integer, f20 integer, f21 integer, f22 integer, f23 integer, f24 integer, f25 integer, f26 integer, f27 integer, f28 integer, f29 integer, f30 integer, f31 integer, f32 integer, f33 integer, f34 integer, f35 integer, f36 integer, f37 integer, f38 integer, f39 integer, f40 integer, f41 integer, f42 integer, f43 integer, f44 integer, f45 integer, f46 integer, f47 integer, f48 integer, f49 integer, f50 integer, f51 integer, f52 integer, f53 integer, f54 integer, f55 integer, f56 integer, f57 integer, f58 integer, f59 integer, f60 integer, f61 integer, f62 integer, f63 integer) LANGUAGE SQL AS 'SELECT 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63';
SELECT f0, f63 FROM differential_wide_result();
SELECT cardinality(proallargtypes), cardinality(proargmodes), cardinality(proargnames), proargmodes[1], proargmodes[64] FROM pg_proc WHERE proname = 'differential_wide_result';
CREATE FUNCTION differential_single_result() RETURNS TABLE (v integer) LANGUAGE SQL AS 'SELECT 7';
CREATE FUNCTION differential_wide_both(a0 integer DEFAULT 1, a1 integer DEFAULT 1, a2 integer DEFAULT 1, a3 integer DEFAULT 1, a4 integer DEFAULT 1, a5 integer DEFAULT 1, a6 integer DEFAULT 1, a7 integer DEFAULT 1, a8 integer DEFAULT 1, a9 integer DEFAULT 1, a10 integer DEFAULT 1, a11 integer DEFAULT 1, a12 integer DEFAULT 1, a13 integer DEFAULT 1, a14 integer DEFAULT 1, a15 integer DEFAULT 1, a16 integer DEFAULT 1, a17 integer DEFAULT 1, a18 integer DEFAULT 1, a19 integer DEFAULT 1, a20 integer DEFAULT 1, a21 integer DEFAULT 1, a22 integer DEFAULT 1, a23 integer DEFAULT 1, a24 integer DEFAULT 1, a25 integer DEFAULT 1, a26 integer DEFAULT 1, a27 integer DEFAULT 1, a28 integer DEFAULT 1, a29 integer DEFAULT 1, a30 integer DEFAULT 1, a31 integer DEFAULT 1, a32 integer DEFAULT 1, a33 integer DEFAULT 1, a34 integer DEFAULT 1, a35 integer DEFAULT 1, a36 integer DEFAULT 1, a37 integer DEFAULT 1, a38 integer DEFAULT 1, a39 integer DEFAULT 1, a40 integer DEFAULT 1, a41 integer DEFAULT 1, a42 integer DEFAULT 1, a43 integer DEFAULT 1, a44 integer DEFAULT 1, a45 integer DEFAULT 1, a46 integer DEFAULT 1, a47 integer DEFAULT 1, a48 integer DEFAULT 1, a49 integer DEFAULT 1, a50 integer DEFAULT 1, a51 integer DEFAULT 1, a52 integer DEFAULT 1, a53 integer DEFAULT 1, a54 integer DEFAULT 1, a55 integer DEFAULT 1, a56 integer DEFAULT 1, a57 integer DEFAULT 1, a58 integer DEFAULT 1, a59 integer DEFAULT 1, a60 integer DEFAULT 1, a61 integer DEFAULT 1, a62 integer DEFAULT 1, a63 integer DEFAULT 1, a64 integer DEFAULT 1, a65 integer DEFAULT 1, a66 integer DEFAULT 1, a67 integer DEFAULT 1, a68 integer DEFAULT 1, a69 integer DEFAULT 1, a70 integer DEFAULT 1, a71 integer DEFAULT 1, a72 integer DEFAULT 1, a73 integer DEFAULT 1, a74 integer DEFAULT 1, a75 integer DEFAULT 1, a76 integer DEFAULT 1, a77 integer DEFAULT 1, a78 integer DEFAULT 1, a79 integer DEFAULT 1, a80 integer DEFAULT 1, a81 integer DEFAULT 1, a82 integer DEFAULT 1, a83 integer DEFAULT 1, a84 integer DEFAULT 1, a85 integer DEFAULT 1, a86 integer DEFAULT 1, a87 integer DEFAULT 1, a88 integer DEFAULT 1, a89 integer DEFAULT 1, a90 integer DEFAULT 1, a91 integer DEFAULT 1, a92 integer DEFAULT 1, a93 integer DEFAULT 1, a94 integer DEFAULT 1, a95 integer DEFAULT 1, a96 integer DEFAULT 1, a97 integer DEFAULT 1, a98 integer DEFAULT 1, a99 integer DEFAULT 1) RETURNS TABLE (f0 integer, f1 integer, f2 integer, f3 integer, f4 integer, f5 integer, f6 integer, f7 integer, f8 integer, f9 integer, f10 integer, f11 integer, f12 integer, f13 integer, f14 integer, f15 integer, f16 integer, f17 integer, f18 integer, f19 integer, f20 integer, f21 integer, f22 integer, f23 integer, f24 integer, f25 integer, f26 integer, f27 integer, f28 integer, f29 integer, f30 integer, f31 integer, f32 integer, f33 integer, f34 integer, f35 integer, f36 integer, f37 integer, f38 integer, f39 integer, f40 integer, f41 integer, f42 integer, f43 integer, f44 integer, f45 integer, f46 integer, f47 integer, f48 integer, f49 integer, f50 integer, f51 integer, f52 integer, f53 integer, f54 integer, f55 integer, f56 integer, f57 integer, f58 integer, f59 integer, f60 integer, f61 integer, f62 integer, f63 integer) LANGUAGE SQL AS 'SELECT 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63';
SELECT cardinality(proallargtypes), cardinality(proargmodes), cardinality(proargnames), proargmodes[100], proargmodes[101], proargmodes[164] FROM pg_proc WHERE proname = 'differential_wide_both';
SELECT f0, f63 FROM differential_wide_both();
SELECT prorettype = 'integer'::regtype, proargmodes::text, proargnames::text FROM pg_proc WHERE proname = 'differential_single_result';
CREATE FUNCTION differential_wide_settings() RETURNS text LANGUAGE SQL SET application_name TO 'wide_settings' SET search_path TO 'public' SET DateStyle TO 'ISO, MDY' SET IntervalStyle TO 'postgres' SET TimeZone TO 'UTC' SET client_encoding TO 'UTF8' SET client_min_messages TO 'notice' SET extra_float_digits TO '1' SET lock_timeout TO '0' SET statement_timeout TO '0' SET row_security TO 'on' SET bytea_output TO 'hex' SET check_function_bodies TO 'on' SET default_transaction_isolation TO 'read committed' SET default_transaction_read_only TO 'off' SET default_transaction_deferrable TO 'off' SET standard_conforming_strings TO 'on' SET xmloption TO 'content' AS 'SELECT current_setting(''application_name'')';
SELECT cardinality(proconfig) FROM pg_proc WHERE proname = 'differential_wide_settings';
SELECT differential_wide_settings();
CREATE TABLE differential_wide_audit (n integer, first text, last text, lower_index integer, upper_index integer, missing boolean, empty boolean);
CREATE TABLE differential_wide_target (v integer);
CREATE FUNCTION differential_wide_trigger() RETURNS trigger LANGUAGE plpgsql AS
'BEGIN INSERT INTO differential_wide_audit VALUES (TG_NARGS, TG_ARGV[0], TG_ARGV[63], array_lower(TG_ARGV,1), array_upper(TG_ARGV,1), TG_ARGV[64] IS NULL, TG_ARGV IS NULL); RETURN NEW; END';
CREATE TRIGGER differential_arguments BEFORE INSERT ON differential_wide_target FOR EACH ROW EXECUTE FUNCTION differential_wide_trigger('argument_0', 'argument_1', 'argument_2', 'argument_3', 'argument_4', 'argument_5', 'argument_6', 'argument_7', 'argument_8', 'argument_9', 'argument_10', 'argument_11', 'argument_12', 'argument_13', 'argument_14', 'argument_15', 'argument_16', 'argument_17', 'argument_18', 'argument_19', 'argument_20', 'argument_21', 'argument_22', 'argument_23', 'argument_24', 'argument_25', 'argument_26', 'argument_27', 'argument_28', 'argument_29', 'argument_30', 'argument_31', 'argument_32', 'argument_33', 'argument_34', 'argument_35', 'argument_36', 'argument_37', 'argument_38', 'argument_39', 'argument_40', 'argument_41', 'argument_42', 'argument_43', 'argument_44', 'argument_45', 'argument_46', 'argument_47', 'argument_48', 'argument_49', 'argument_50', 'argument_51', 'argument_52', 'argument_53', 'argument_54', 'argument_55', 'argument_56', 'argument_57', 'argument_58', 'argument_59', 'argument_60', 'argument_61', 'argument_62', 'argument_63');
INSERT INTO differential_wide_target VALUES (1),(2),(3);
SELECT * FROM differential_wide_audit ORDER BY n;
DROP TRIGGER differential_arguments ON differential_wide_target;
CREATE TRIGGER differential_no_arguments BEFORE INSERT ON differential_wide_target FOR EACH ROW EXECUTE FUNCTION differential_wide_trigger();
INSERT INTO differential_wide_target VALUES (4);
SELECT * FROM differential_wide_audit WHERE n = 0;
CREATE ROLE differential_policy_role_0;
CREATE ROLE differential_policy_role_1;
CREATE ROLE differential_policy_role_2;
CREATE ROLE differential_policy_role_3;
CREATE ROLE differential_policy_role_4;
CREATE ROLE differential_policy_role_5;
CREATE ROLE differential_policy_role_6;
CREATE ROLE differential_policy_role_7;
CREATE ROLE differential_policy_role_8;
GRANT ALL ON differential_wide_target TO differential_policy_role_0;
CREATE POLICY differential_allow ON differential_wide_target TO differential_policy_role_0, differential_policy_role_1, differential_policy_role_2, differential_policy_role_3, differential_policy_role_4, differential_policy_role_5, differential_policy_role_6, differential_policy_role_7, differential_policy_role_8 USING (v > 0) WITH CHECK (v > 0);
CREATE POLICY differential_restrict_1 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_2 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_3 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_4 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_5 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_6 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_7 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_8 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_9 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_10 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_11 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_12 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_13 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_14 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_15 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_16 ON differential_wide_target AS RESTRICTIVE USING (v < 100) WITH CHECK (v < 100);
CREATE POLICY differential_restrict_17 ON differential_wide_target AS RESTRICTIVE USING (v < 3) WITH CHECK (v < 3);
ALTER TABLE differential_wide_target ENABLE ROW LEVEL SECURITY;
SELECT count(*) FROM pg_policy WHERE polname LIKE 'differential_%';
SELECT cardinality(polroles) FROM pg_policy WHERE polname = 'differential_allow';
SET ROLE differential_policy_role_0;
SELECT v FROM differential_wide_target ORDER BY v;
UPDATE differential_wide_target SET v = v + 10;
RESET ROLE;
BEGIN;
ALTER POLICY differential_restrict_17 ON differential_wide_target USING (false);
ROLLBACK;
SET ROLE differential_policy_role_0;
SELECT v FROM differential_wide_target ORDER BY v;
RESET ROLE;
