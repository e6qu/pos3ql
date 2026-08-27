SELECT oid, proname, pronamespace, prorettype, proowner, prolang, prosrc
  FROM pg_proc
 WHERE oid IN (89, 2079, 2081)
 ORDER BY oid;

SELECT pg_function_is_visible(89),
       pg_function_is_visible(999999),
       pg_table_is_visible('pg_proc'::regclass),
       pg_type_is_visible(26),
       pg_collation_is_visible(100),
       pg_relation_is_publishable('pg_proc'::regclass);

SELECT oid, proname, prorettype, proargtypes, proisstrict, provolatile, proparallel
  FROM pg_proc
 WHERE oid IN (1158, 1768, 1770, 1772, 1773, 1774, 1775, 1776, 1777, 1778, 1780, 2049)
 ORDER BY oid;

SELECT 'to_char(timestamp with time zone,text)'::regprocedure::oid,
       'to_char(timestamp without time zone,text)'::regprocedure::oid,
       'to_number(text,text)'::regprocedure::oid,
       'to_date(text,text)'::regprocedure::oid,
       'to_timestamp(text,text)'::regprocedure::oid;

SELECT oid, proname, proargtypes, proisstrict, provolatile, proparallel
  FROM pg_proc
 WHERE oid IN (1081, 1402, 1641, 2078, 2730, 3086)
 ORDER BY oid;
