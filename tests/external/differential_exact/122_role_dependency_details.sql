-- DROP ROLE lists policies naming the role and default privileges it owns
-- or receives; pg_shdepend holds the same rows.
CREATE ROLE dr_a;
CREATE ROLE dr_b;
CREATE TABLE dr_t1(a integer);
ALTER TABLE dr_t1 OWNER TO dr_a;
CREATE POLICY dr_p ON dr_t1 TO dr_b USING (true);
ALTER DEFAULT PRIVILEGES FOR ROLE dr_a GRANT SELECT ON TABLES TO dr_b;
ALTER DEFAULT PRIVILEGES FOR ROLE dr_b IN SCHEMA public GRANT USAGE ON SEQUENCES TO dr_a;
DROP ROLE dr_a;
DROP ROLE dr_b;
SELECT classid::regclass, deptype, refobjid::regrole
  FROM pg_shdepend WHERE refobjid IN ('dr_a'::regrole, 'dr_b'::regrole)
 ORDER BY 1, 2, 3;
SELECT pg_describe_object(classid, objid, objsubid)
  FROM pg_shdepend WHERE refobjid IN ('dr_a'::regrole, 'dr_b'::regrole)
 ORDER BY 1;
DROP POLICY dr_p ON dr_t1;
DROP TABLE dr_t1;
ALTER DEFAULT PRIVILEGES FOR ROLE dr_a REVOKE SELECT ON TABLES FROM dr_b;
ALTER DEFAULT PRIVILEGES FOR ROLE dr_b IN SCHEMA public REVOKE USAGE ON SEQUENCES FROM dr_a;
DROP ROLE dr_a, dr_b;
