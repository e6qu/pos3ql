-- COPY: the bulk-data subprotocol, text format. FROM STDIN with inline
-- data (psql feeds it through the wire's CopyData messages, ending on the
-- \. marker), TO STDOUT with PostgreSQL's exact escaping, NULLs, column
-- lists, defaults and sequences filling unlisted columns, constraint
-- enforcement mid-stream, and transactional behavior (a COPY inside BEGIN
-- rolls back with it; a failed COPY stores nothing).
DROP TABLE IF EXISTS copy_basic;
CREATE TABLE copy_basic (a int, b text, c text);
COPY copy_basic FROM STDIN;
1	hello	world
2	tab\there	\N
3	line\nbreak	back\\slash
4	cr\rreturn	octal\101byte
5		empty before this
\.
SELECT a, b, c FROM copy_basic ORDER BY a;
COPY copy_basic TO STDOUT;
COPY copy_basic (c, a) TO STDOUT;

-- Escapes round-trip: what COPY TO printed, COPY FROM must re-read to the
-- same values.
DROP TABLE IF EXISTS copy_echo;
CREATE TABLE copy_echo (a int, b text, c text);
COPY copy_echo FROM STDIN;
2	tab\there	\N
3	line\nbreak	back\\slash
\.
SELECT (SELECT b FROM copy_basic WHERE a = 2) = (SELECT b FROM copy_echo WHERE a = 2) AS tab_round_trip;
SELECT (SELECT b FROM copy_basic WHERE a = 3) = (SELECT b FROM copy_echo WHERE a = 3) AS newline_round_trip;

-- Typed columns run each type's input function; defaults and serials fill
-- what the column list omits.
DROP TABLE IF EXISTS copy_typed;
CREATE TABLE copy_typed (
  id serial,
  n numeric(8,2),
  t timestamp,
  u uuid,
  arr int[],
  flag bool DEFAULT true,
  note text DEFAULT 'unset'
);
COPY copy_typed (n, t, u, arr) FROM STDIN;
12.35	2024-02-29 12:34:56	a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11	{1,2,3}
\N	\N	\N	\N
\.
SELECT id, n, t, u, arr, flag, note FROM copy_typed ORDER BY id;
COPY copy_typed TO STDOUT;

-- Constraint failures abort the whole COPY: nothing from the stream stays.
DROP TABLE IF EXISTS copy_pk;
CREATE TABLE copy_pk (id int PRIMARY KEY, v text);
COPY copy_pk FROM STDIN;
1	first
2	second
1	duplicate
\.
SELECT count(*) AS after_failed_copy FROM copy_pk;

-- Bad input syntax mid-stream aborts too.
COPY copy_pk FROM STDIN;
7	fine
eight	not a number
\.
SELECT count(*) AS after_bad_int FROM copy_pk;

-- Field-count mismatches are 22P04-class errors.
COPY copy_pk FROM STDIN;
1	too	many	fields
\.
COPY copy_pk FROM STDIN;
1
\.
SELECT count(*) AS still_empty FROM copy_pk;

-- Inside an explicit transaction, COPY commits and rolls back with it.
BEGIN;
COPY copy_pk FROM STDIN;
10	kept
\.
COMMIT;
BEGIN;
COPY copy_pk FROM STDIN;
11	discarded
\.
ROLLBACK;
SELECT id, v FROM copy_pk ORDER BY id;

DROP TABLE copy_basic;
DROP TABLE copy_echo;
DROP TABLE copy_typed;
DROP TABLE copy_pk;
