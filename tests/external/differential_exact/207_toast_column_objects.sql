-- TOAST relation columns and missing relation attributes in object descriptions.
CREATE TABLE tt (a text);
SELECT regexp_replace(pg_describe_object(classid, objid, objsubid), '[0-9]+', 'N', 'g') AS dependent, regexp_replace(pg_describe_object(refclassid, refobjid, refobjsubid), '[0-9]+', 'N', 'g') AS referenced, deptype FROM pg_depend d JOIN pg_class c ON c.oid = d.objid AND d.classid = 'pg_class'::regclass WHERE c.relnamespace = 'pg_toast'::regnamespace AND (refobjid = 'tt'::regclass OR refobjid = (SELECT reltoastrelid FROM pg_class WHERE oid = 'tt'::regclass)) ORDER BY 1, 2;
SELECT (pg_identify_object('pg_class'::regclass, reltoastrelid, a)).type, regexp_replace((pg_identify_object('pg_class'::regclass, reltoastrelid, a)).identity, '[0-9]+', 'N', 'g') FROM pg_class, generate_series(1, 4) a WHERE oid = 'tt'::regclass ORDER BY a;
SELECT (pg_identify_object_as_address('pg_class'::regclass, reltoastrelid, 2)).type FROM pg_class WHERE oid = 'tt'::regclass;
DROP TABLE tt;
CREATE TABLE zz (a int);
CREATE VIEW zv AS SELECT 1 AS x;
SELECT pg_identify_object('pg_class'::regclass, 'zz'::regclass, 9), pg_describe_object('pg_class'::regclass, 'zz'::regclass, 9);
SELECT pg_identify_object('pg_class'::regclass, 'zv'::regclass, 9), pg_describe_object('pg_class'::regclass, 'zv'::regclass, 9);
DROP TABLE zz; DROP VIEW zv;
