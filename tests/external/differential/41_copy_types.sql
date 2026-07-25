-- COPY across the type surface: every supported type out through COPY TO
-- (the wire output function plus COPY escapes) and back in through COPY
-- FROM (the input function), with escape-hostile and boundary values.
-- Whatever COPY TO prints, COPY FROM must re-read to an equal value — and
-- both sides must match real PostgreSQL byte for byte.
DROP TABLE IF EXISTS ct_num;
CREATE TABLE ct_num (
  b bool, i2 smallint, i4 int, i8 bigint,
  f4 real, f8 double precision, n numeric(20,5)
);
COPY ct_num FROM STDIN;
t	-32768	-2147483648	-9223372036854775808	0.5	2.25	-12345.67890
f	32767	2147483647	9223372036854775807	-1.5	1e300	0.00001
\N	\N	\N	\N	\N	\N	\N
\.
COPY ct_num TO STDOUT;
SELECT * FROM ct_num;

DROP TABLE IF EXISTS ct_text;
CREATE TABLE ct_text (t text, v varchar(20), c char(6), nm name, by bytea);
COPY ct_text FROM STDIN;
plain	short	pad	a_name	\\xdeadbeef
has\ttab	q'uote	six 66	x	\\x00ff10
multi\nline	back\\slash	\N	\N	\N
\.
COPY ct_text TO STDOUT;
SELECT t, v, c, octet_length(c) AS c_len, nm, by FROM ct_text;

DROP TABLE IF EXISTS ct_time;
CREATE TABLE ct_time (
  d date, tm time, ttz timetz, ts timestamp, tstz timestamptz, iv interval
);
COPY ct_time FROM STDIN;
2024-02-29	23:59:59.999999	12:00:00-05	2024-02-29 12:34:56.789	2024-02-29 12:34:56+02	1 year 2 mons 3 days 04:05:06
1999-12-31	00:00:00	00:00:00+00	1999-12-31 23:59:59	2000-01-01 00:00:00Z	-3 days
\.
COPY ct_time TO STDOUT;
SELECT * FROM ct_time;

DROP TABLE IF EXISTS ct_struct;
CREATE TABLE ct_struct (
  u uuid, j json, jb jsonb, ia int[], ta text[],
  r int4range, mr int4multirange, bt bit(4), vb varbit
);
COPY ct_struct FROM STDIN;
a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11	{"k": "v\\tt"}	{"z": 1, "a": [1,2]}	{1,2,3}	{"a b","c\\"d","e\\\\tf"}	[1,10)	{[1,3),[5,9)}	1010	101101
\N	\N	\N	{}	{NULL,"x"}	empty	{}	0000	0
\.
COPY ct_struct TO STDOUT;
SELECT * FROM ct_struct;

-- Round trip: re-load what COPY TO of ct_text printed (the same lines,
-- verbatim) and compare value-for-value.
DROP TABLE IF EXISTS ct_text2;
CREATE TABLE ct_text2 (t text, v varchar(20), c char(6), nm name, by bytea);
COPY ct_text2 FROM STDIN;
plain	short	pad   	a_name	\\xdeadbeef
has\ttab	q'uote	six 66	x	\\x00ff10
multi\nline	back\\slash	\N	\N	\N
\.
SELECT count(*) AS text_round_trip
FROM ct_text a JOIN ct_text2 b
  ON a.t = b.t
 AND a.v IS NOT DISTINCT FROM b.v
 AND a.c IS NOT DISTINCT FROM b.c
 AND a.nm IS NOT DISTINCT FROM b.nm
 AND a.by IS NOT DISTINCT FROM b.by;

-- Serials advance for COPY like INSERT; a COPY-supplied id does not move
-- the sequence (PostgreSQL's rule: only default assignment advances it).
DROP TABLE IF EXISTS ct_serial;
CREATE TABLE ct_serial (id serial, v text);
COPY ct_serial (v) FROM STDIN;
first
second
\.
COPY ct_serial (id, v) FROM STDIN;
100	explicit
\.
COPY ct_serial (v) FROM STDIN;
third
\.
SELECT id, v FROM ct_serial ORDER BY id;

DROP TABLE ct_num;
DROP TABLE ct_text;
DROP TABLE ct_time;
DROP TABLE ct_struct;
DROP TABLE ct_text2;
DROP TABLE ct_serial;
