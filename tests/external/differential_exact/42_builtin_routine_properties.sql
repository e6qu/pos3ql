-- Registered routine identities and properties use the PostgreSQL 18.6 catalog.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/include/catalog/pg_proc.dat
SELECT oid,proname,prokind,proisstrict,prorettype,proargtypes,provolatile
FROM pg_catalog.pg_proc WHERE oid IN
(2242,2243,2621,2943,2944,2945,2946,2947,2948,3348,3360,3545,3566,
 3840,3841,3844,3845,3933,3934,3937,3938,3941,3942,3945,3946,4568,
 5059,5060,5061,5062,5063,5064,5065,5066,6167,6395,6396) ORDER BY oid;
