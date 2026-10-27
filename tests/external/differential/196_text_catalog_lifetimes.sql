CREATE COLLATION text_lifetime_byte (PROVIDER = libc, LOCALE = 'C');
CREATE CONVERSION text_lifetime_convert FOR 'LATIN1' TO 'UTF8' FROM pg_catalog.iso8859_1_to_utf8;
CREATE TEXT SEARCH PARSER text_lifetime_parser (
  START = prsd_start, GETTOKEN = prsd_nexttoken, END = prsd_end,
  HEADLINE = prsd_headline, LEXTYPES = prsd_lextype);
CREATE TEXT SEARCH TEMPLATE text_lifetime_template (INIT = dsimple_init, LEXIZE = dsimple_lexize);
CREATE TEXT SEARCH DICTIONARY text_lifetime_dictionary (TEMPLATE = text_lifetime_template, ACCEPT = true);
CREATE TEXT SEARCH CONFIGURATION text_lifetime_config (PARSER = text_lifetime_parser);
ALTER TEXT SEARCH CONFIGURATION text_lifetime_config ADD MAPPING
  FOR asciiword, word, numword, uint WITH text_lifetime_dictionary;
COMMENT ON COLLATION text_lifetime_byte IS 'original collation';
COMMENT ON CONVERSION text_lifetime_convert IS 'original conversion';
COMMENT ON TEXT SEARCH CONFIGURATION text_lifetime_config IS 'original search';
CREATE VIEW text_lifetime_catalog AS
  SELECT 'collation' AS kind, oid AS identity FROM pg_collation WHERE collname = 'text_lifetime_byte'
  UNION ALL SELECT 'conversion', oid FROM pg_conversion WHERE conname = 'text_lifetime_convert'
  UNION ALL SELECT 'parser', oid FROM pg_ts_parser WHERE prsname = 'text_lifetime_parser'
  UNION ALL SELECT 'template', oid FROM pg_ts_template WHERE tmplname = 'text_lifetime_template'
  UNION ALL SELECT 'dictionary', oid FROM pg_ts_dict WHERE dictname = 'text_lifetime_dictionary'
  UNION ALL SELECT 'configuration', oid FROM pg_ts_config WHERE cfgname = 'text_lifetime_config';
CREATE TABLE saved_text_lifetimes AS SELECT * FROM text_lifetime_catalog;
SELECT count(*) = 6 FROM saved_text_lifetimes;
SELECT to_tsvector('text_lifetime_config', 'Cats 42') = '''42'':2 ''cats'':1'::tsvector;
SELECT obj_description(oid, 'pg_collation') = 'original collation' FROM pg_collation WHERE collname = 'text_lifetime_byte';
SELECT obj_description(oid, 'pg_conversion') = 'original conversion' FROM pg_conversion WHERE conname = 'text_lifetime_convert';
-- replacement checks
DROP COLLATION text_lifetime_byte;
CREATE COLLATION text_lifetime_byte (PROVIDER = libc, LOCALE = 'C');
DROP CONVERSION text_lifetime_convert;
CREATE CONVERSION text_lifetime_convert FOR 'LATIN1' TO 'UTF8' FROM pg_catalog.iso8859_1_to_utf8;
DROP TEXT SEARCH CONFIGURATION text_lifetime_config;
DROP TEXT SEARCH DICTIONARY text_lifetime_dictionary;
DROP TEXT SEARCH TEMPLATE text_lifetime_template;
DROP TEXT SEARCH PARSER text_lifetime_parser;
CREATE TEXT SEARCH PARSER text_lifetime_parser (
  START = prsd_start, GETTOKEN = prsd_nexttoken, END = prsd_end,
  HEADLINE = prsd_headline, LEXTYPES = prsd_lextype);
CREATE TEXT SEARCH TEMPLATE text_lifetime_template (INIT = dsimple_init, LEXIZE = dsimple_lexize);
CREATE TEXT SEARCH DICTIONARY text_lifetime_dictionary (TEMPLATE = text_lifetime_template, ACCEPT = true);
CREATE TEXT SEARCH CONFIGURATION text_lifetime_config (PARSER = text_lifetime_parser);
ALTER TEXT SEARCH CONFIGURATION text_lifetime_config ADD MAPPING
  FOR asciiword, word, numword, uint WITH text_lifetime_dictionary;
SELECT count(*) = 0 FROM saved_text_lifetimes s JOIN text_lifetime_catalog c ON s.kind = c.kind AND s.identity = c.identity;
SELECT obj_description(oid, 'pg_collation') IS NULL FROM pg_collation WHERE collname = 'text_lifetime_byte';
SELECT obj_description(oid, 'pg_conversion') IS NULL FROM pg_conversion WHERE conname = 'text_lifetime_convert';
SELECT obj_description(oid, 'pg_ts_config') IS NULL FROM pg_ts_config WHERE cfgname = 'text_lifetime_config';
BEGIN;
SAVEPOINT before_rename;
ALTER COLLATION text_lifetime_byte RENAME TO text_lifetime_renamed;
ALTER CONVERSION text_lifetime_convert RENAME TO text_lifetime_convert_renamed;
ALTER TEXT SEARCH CONFIGURATION text_lifetime_config RENAME TO text_lifetime_config_renamed;
ROLLBACK TO before_rename;
COMMIT;
SELECT count(*) = 6 FROM text_lifetime_catalog;
DROP VIEW text_lifetime_catalog;
DROP TABLE saved_text_lifetimes;
DROP COLLATION text_lifetime_byte;
DROP CONVERSION text_lifetime_convert;
DROP TEXT SEARCH CONFIGURATION text_lifetime_config;
DROP TEXT SEARCH DICTIONARY text_lifetime_dictionary;
DROP TEXT SEARCH TEMPLATE text_lifetime_template;
DROP TEXT SEARCH PARSER text_lifetime_parser;
