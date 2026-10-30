-- PostgreSQL 18: SPI_cursor_open_internal rejects zero or multiple commands.
-- https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/executor/spi.c
CREATE SEQUENCE cursor_query_calls;
CREATE FUNCTION cursor_query_count(q text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE c refcursor;
BEGIN
  OPEN c FOR EXECUTE q;
  CLOSE c;
  RETURN;
END $$;
SELECT cursor_query_count('SELECT nextval(''cursor_query_calls''); SELECT 2');
SELECT last_value, is_called FROM cursor_query_calls;
SELECT cursor_query_count('');
DROP FUNCTION cursor_query_count(text);
DROP SEQUENCE cursor_query_calls;

CREATE FUNCTION cursor_captured_locals() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'captured_locals'; label text := 'opening value'; d date;
BEGIN
  OPEN c FOR SELECT label AS label, d AS day, found AS was_found;
  label := 'changed';
  d := DATE '2026-10-30';
  RETURN c;
END $$;
BEGIN;
SELECT cursor_captured_locals();
FETCH ALL FROM captured_locals;
CLOSE captured_locals;
COMMIT;
DROP FUNCTION cursor_captured_locals();

-- A record wider than one byte must not lose fields or the following column.
BEGIN;
DECLARE wide_record SCROLL CURSOR FOR SELECT ROW(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 255, 256) AS value, 99 AS following;
FETCH NEXT FROM wide_record;
COMMIT;

-- https://www.postgresql.org/docs/18/typeconv-union-case.html
CREATE DOMAIN cursor_day AS date;
SELECT pg_typeof(day) FROM (SELECT NULL::cursor_day AS day UNION ALL SELECT NULL::cursor_day) AS same_domain LIMIT 1;
SELECT pg_typeof(day) FROM (SELECT NULL::cursor_day AS day UNION ALL SELECT NULL) AS mixed_domain LIMIT 1;
CREATE FUNCTION cursor_set_locals(p integer) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'set_locals'; label text := 'opening value'; d cursor_day;
BEGIN
  OPEN c FOR
    SELECT label AS label, d AS day, p AS input
    UNION ALL SELECT label, d, p + 1
    ORDER BY input;
  label := 'changed';
  p := 900;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_set_locals(17);
FETCH ALL FROM set_locals;
CLOSE set_locals;
COMMIT;
DROP FUNCTION cursor_set_locals(integer);
SELECT pg_typeof(f2), pg_typeof(day) FROM (
  SELECT (ROW(1, 2)).*, NULL::cursor_day AS day
  UNION ALL SELECT (ROW(3, 4)).*, NULL::cursor_day
) AS expanded_record LIMIT 1;
CREATE TABLE cursor_domain_source(day cursor_day);
INSERT INTO cursor_domain_source VALUES (NULL);
SELECT pg_typeof(day) FROM (
  SELECT * FROM cursor_domain_source UNION ALL SELECT * FROM cursor_domain_source
) AS expanded_table LIMIT 1;
DROP TABLE cursor_domain_source;
DROP DOMAIN cursor_day;

CREATE TABLE cursor_local_source(id integer);
INSERT INTO cursor_local_source VALUES (1), (2), (3);
CREATE FUNCTION cursor_table_local() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'table_local'; label text := 'opening value'; cutoff integer := 2;
BEGIN
  OPEN c FOR SELECT id, label, label::text FROM cursor_local_source WHERE id <= cutoff ORDER BY id;
  label := 'changed';
  cutoff := 0;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_table_local();
FETCH ALL FROM table_local;
CLOSE table_local;
COMMIT;
DROP FUNCTION cursor_table_local();
DROP TABLE cursor_local_source;

CREATE TABLE cursor_ambiguous_source(id integer);
INSERT INTO cursor_ambiguous_source VALUES (1);
CREATE SEQUENCE cursor_ambiguous_calls;
CREATE FUNCTION cursor_ambiguous_local() RETURNS void LANGUAGE plpgsql AS $$
DECLARE c refcursor; id integer := 10;
BEGIN
  OPEN c FOR SELECT nextval('cursor_ambiguous_calls'), id FROM cursor_ambiguous_source;
  CLOSE c;
END $$;
SELECT cursor_ambiguous_local();
SELECT last_value, is_called FROM cursor_ambiguous_calls;
DROP FUNCTION cursor_ambiguous_local();
DROP TABLE cursor_ambiguous_source;
DROP SEQUENCE cursor_ambiguous_calls;

CREATE FUNCTION cursor_record_local() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'record_local'; r record;
BEGIN
  SELECT 'opening value'::text AS label INTO r;
  OPEN c FOR SELECT r.label AS label UNION ALL SELECT r.label;
  SELECT 'changed'::text AS label INTO r;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_record_local();
FETCH ALL FROM record_local;
CLOSE record_local;
COMMIT;
DROP FUNCTION cursor_record_local();

CREATE FUNCTION cursor_wide_record_field() RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := 'wide_record_field'; r record;
BEGIN
  SELECT ROW(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 255, 256) AS payload INTO r;
  OPEN c FOR SELECT ((r.payload).f256)::integer;
  RETURN c;
END $$;
BEGIN;
SELECT cursor_wide_record_field();
FETCH ALL FROM wide_record_field;
CLOSE wide_record_field;
COMMIT;
DROP FUNCTION cursor_wide_record_field();
SELECT (ROW(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65)).*;
