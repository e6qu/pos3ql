-- PostgreSQL 18 refcursor identity, session cursor catalogs, SQL cursor
-- metadata, and native PL/pgSQL cursor control.
DROP TABLE IF EXISTS refcursor_values;
DROP FUNCTION IF EXISTS refcursor_native_probe(integer);
DROP FUNCTION IF EXISTS refcursor_returned_probe(integer);
DROP FUNCTION IF EXISTS refcursor_record_probe();

CREATE TABLE refcursor_values(id integer, handle refcursor, handles refcursor[]);
INSERT INTO refcursor_values VALUES
  (1, 'saved', ARRAY['left','right']::refcursor[]),
  (2, NULL, NULL);
SELECT id, handle, handles, pg_typeof(handle), pg_typeof(handles)
  FROM refcursor_values ORDER BY id;
SELECT oid, typname, typlen, typtype, typcategory, typelem, typarray, typinput, typoutput
  FROM pg_type WHERE oid IN (1790,2201) ORDER BY oid;
SELECT oid, proname, prorettype, proretset, proallargtypes, proargmodes, proargnames,
       provolatile, proparallel
  FROM pg_proc WHERE oid = 2511;
SELECT oid, reltype, relkind, relnatts, relhasrules, relispopulated
  FROM pg_class WHERE oid = 12077;
SELECT attname, atttypid, attlen, attnum, attcollation, attstorage, attalign
  FROM pg_attribute WHERE attrelid = 12077 AND attnum > 0 ORDER BY attnum;
SELECT (SELECT count(*) FROM pg_operator
          WHERE oprleft IN (1790,2201) OR oprright IN (1790,2201)) AS operators,
       (SELECT count(*) FROM pg_opclass WHERE opcintype IN (1790,2201)) AS opclasses,
       (SELECT count(*) FROM pg_cast
          WHERE castsource IN (1790,2201) OR casttarget IN (1790,2201)) AS casts;

BEGIN;
DECLARE visible BINARY SCROLL CURSOR WITH HOLD FOR
  SELECT id, handle FROM refcursor_values ORDER BY id;
SELECT name, statement LIKE 'DECLARE visible%' AS statement_is_declare,
       is_holdable, is_binary, is_scrollable, creation_time IS NOT NULL
  FROM pg_cursors;
SELECT name, is_holdable, is_binary, is_scrollable, creation_time IS NOT NULL
  FROM pg_cursor();
FETCH NEXT FROM visible;
MOVE LAST FROM visible;
FETCH PRIOR FROM visible;
COMMIT;
FETCH NEXT FROM visible;
CLOSE visible;

CREATE FUNCTION refcursor_native_probe(n integer) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
  c refcursor := 'native';
  first_value text;
  prior_value text;
BEGIN
  OPEN c SCROLL FOR EXECUTE
    'SELECT $1::text UNION ALL SELECT ''b''' USING n;
  FETCH NEXT FROM c INTO first_value;
  MOVE LAST FROM c;
  FETCH PRIOR FROM c INTO prior_value;
  CLOSE c;
  RETURN first_value || ':' || prior_value;
END
$$;
SELECT refcursor_native_probe(7);

CREATE FUNCTION refcursor_record_probe() RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
  c refcursor := 'record';
  value record;
BEGIN
  OPEN c FOR SELECT 4::integer AS number, 'v'::text AS label;
  FETCH NEXT FROM c INTO value;
  CLOSE c;
  RETURN value.number::text || ':' || value.label;
END
$$;
SELECT refcursor_record_probe();

CREATE FUNCTION refcursor_returned_probe(n integer) RETURNS refcursor
LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'returned';
BEGIN
  OPEN c FOR SELECT n::integer AS value;
  RETURN c;
END
$$;
BEGIN;
SELECT refcursor_returned_probe(11);
SELECT name, statement, is_holdable, is_binary, is_scrollable
  FROM pg_cursors WHERE name = 'returned';
FETCH NEXT FROM returned;
CLOSE returned;
COMMIT;

SELECT 'a'::refcursor = 'a'::refcursor;
SELECT ARRAY['a']::refcursor[] = ARRAY['a']::refcursor[];
SELECT 'a'::refcursor IN ('a'::refcursor);
SELECT 'a'::refcursor = ANY (ARRAY['a']::refcursor[]);
SELECT CASE 'a'::refcursor WHEN 'a'::refcursor THEN 1 END;
SELECT 'a'::refcursor UNION SELECT 'b'::refcursor;
SELECT 'a'::refcursor UNION ALL SELECT 'b'::refcursor ORDER BY 1;
SELECT DISTINCT handle FROM refcursor_values;
SELECT handle FROM refcursor_values ORDER BY handle;

DROP FUNCTION refcursor_native_probe(integer);
DROP FUNCTION refcursor_returned_probe(integer);
DROP FUNCTION refcursor_record_probe();
DROP TABLE refcursor_values;
