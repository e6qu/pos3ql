-- Named composites and relation rows convert to JSON objects through their
-- catalog fields, and coerce to record arguments.
CREATE TYPE ct AS (a int);
SELECT row_to_json(ROW(1)::ct);
CREATE TABLE rec_plain (a int);
INSERT INTO rec_plain VALUES (1);
SELECT to_jsonb(r) FROM rec_plain r;
SELECT row_to_json(r, true) FROM rec_plain r;
SELECT r FROM rec_plain r;
SELECT pg_typeof(r) FROM rec_plain r;
SELECT json_populate_record(NULL::rec_plain, '{"a":1}');
SELECT to_jsonb(ROW(1)::ct), to_jsonb(ROW(1)), row_to_json(ROW(1));
CREATE TABLE rec (a int, c ct);
INSERT INTO rec VALUES (1, ROW(2));
SELECT to_jsonb(r), to_jsonb(r.*), to_jsonb(c), to_json(c) FROM rec r;
SELECT to_jsonb(ROW(r.a)) FROM rec r;
SELECT row_to_json(r, true) FROM rec r;
SELECT row_to_json(ROW(1, 'a,b', ARRAY[1,2]), true), row_to_json(ROW(1), false), row_to_json(NULL::rec);
SELECT to_json(1.50::numeric), to_jsonb('x'::text), to_json(now()::date - now()::date), to_json('2020-01-02 03:04:05'::timestamp), to_jsonb(ARRAY[ROW(1)::ct]), to_json(NULL::int);
SELECT to_jsonb(r) FROM rec r WHERE false;
DROP TABLE rec_plain;
DROP TABLE rec;
DROP TYPE ct;
