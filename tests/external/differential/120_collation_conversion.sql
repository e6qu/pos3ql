CREATE SCHEMA locale_compat;
CREATE SCHEMA locale_moved;
CREATE COLLATION locale_compat.byte_order
  (PROVIDER = libc, LC_COLLATE = 'C', LC_CTYPE = 'C');
CREATE DEFAULT CONVERSION locale_compat.latin1_to_utf8
  FOR 'LATIN1' TO 'UTF8' FROM pg_catalog.iso8859_1_to_utf8;
COMMENT ON COLLATION locale_compat.byte_order IS 'byte ordering';
COMMENT ON CONVERSION locale_compat.latin1_to_utf8 IS 'latin1 conversion';

CREATE TABLE locale_compat.words (
  id integer,
  value text COLLATE locale_compat.byte_order
);
CREATE INDEX words_value_idx ON locale_compat.words(value);
INSERT INTO locale_compat.words VALUES (1, 'z'), (2, 'a');
CREATE VIEW locale_compat.ordered_words AS
  SELECT value COLLATE locale_compat.byte_order AS value
    FROM locale_compat.words;

SELECT value FROM locale_compat.ordered_words ORDER BY value;
SELECT collprovider, collisdeterministic, collencoding, collcollate, collctype
  FROM pg_collation WHERE collname = 'byte_order';
SELECT conforencoding, contoencoding, conproc::regproc, condefault
  FROM pg_conversion WHERE conname = 'latin1_to_utf8';
SELECT encode(convert_to('é', 'LATIN1'), 'hex'),
       convert_from(decode('e9', 'hex'), 'LATIN1'),
       encode(convert(decode('e9', 'hex'), 'LATIN1', 'UTF8'), 'hex');

ALTER COLLATION locale_compat.byte_order RENAME TO byte_order_renamed;
ALTER COLLATION locale_compat.byte_order_renamed SET SCHEMA locale_moved;
ALTER CONVERSION locale_compat.latin1_to_utf8 RENAME TO latin1_to_utf8_renamed;
ALTER CONVERSION locale_compat.latin1_to_utf8_renamed SET SCHEMA locale_moved;
SELECT collname, n.nspname
  FROM pg_collation c JOIN pg_namespace n ON n.oid = c.collnamespace
 WHERE collname = 'byte_order_renamed';
SELECT conname, n.nspname
  FROM pg_conversion c JOIN pg_namespace n ON n.oid = c.connamespace
 WHERE conname = 'latin1_to_utf8_renamed';
SELECT obj_description(oid, 'pg_collation')
  FROM pg_collation WHERE collname = 'byte_order_renamed';
SELECT obj_description(oid, 'pg_conversion')
  FROM pg_conversion WHERE conname = 'latin1_to_utf8_renamed';
SELECT value FROM locale_compat.ordered_words ORDER BY value;

DROP SCHEMA locale_compat, locale_moved CASCADE;
SELECT count(*) FROM pg_collation WHERE collname = 'byte_order_renamed';
SELECT count(*) FROM pg_conversion WHERE conname = 'latin1_to_utf8_renamed';
