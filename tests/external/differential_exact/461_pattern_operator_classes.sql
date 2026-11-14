-- Pattern operator classes on btree indexes.
CREATE TABLE words (w text, v varchar(10), b char(4), n int4);
CREATE INDEX words_w ON words (w text_pattern_ops);
CREATE INDEX words_v ON words (v varchar_pattern_ops);
CREATE INDEX words_b ON words (b bpchar_pattern_ops);
CREATE INDEX words_wv ON words (w text_pattern_ops, v);
CREATE UNIQUE INDEX words_u ON words (lower(w) text_pattern_ops);
CREATE INDEX words_bad ON words (n text_pattern_ops);
CREATE INDEX words_bad2 ON words (b text_pattern_ops);
CREATE INDEX words_c ON words (w COLLATE "C" text_pattern_ops DESC);
SELECT indexrelid::regclass, pg_get_indexdef(indexrelid), indclass::text, indcollation::text FROM pg_index WHERE indrelid = 'words'::regclass ORDER BY 1;
\d words
INSERT INTO words VALUES ('abc', 'abc', 'abc', 1), ('abd', 'abd', 'abd', 2), ('b', 'b', 'b', 3);
INSERT INTO words VALUES ('ABC', 'x', 'x', 4);
SELECT w FROM words WHERE w LIKE 'ab%' ORDER BY w;
SELECT w FROM words WHERE w ~>=~ 'ab' AND w ~<~ 'ac' ORDER BY w;
SELECT v FROM words WHERE v LIKE 'ab%' ORDER BY v;
SELECT b FROM words WHERE b LIKE 'ab%' ORDER BY b;
SELECT opcname, opcmethod::regclass IS NULL, opcintype::regtype, opcdefault FROM pg_opclass WHERE opcname LIKE '%pattern_ops' ORDER BY 1, 2;
