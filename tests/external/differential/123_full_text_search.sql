DROP SCHEMA IF EXISTS full_text_diff CASCADE;

SELECT 'Fat foo foo:3B foo:2A'::tsvector,
       'fat & (rat | !cat:*AB)'::tsquery;
SELECT to_tsvector('english', 'The Fat Rats'),
       plainto_tsquery('english', 'The Fat Rats'),
       phraseto_tsquery('english', 'The Fat Rats'),
       websearch_to_tsquery('english', '"fat rat" -cat OR dog');
SELECT to_tsvector('english', 'The Fat Rats')
         @@ plainto_tsquery('english', 'fat rat'),
       ts_rank('a:1,2,3'::tsvector, 'a'::tsquery),
       ts_rank_cd('a:1A b:4B'::tsvector, 'a | b'::tsquery, 32);
SELECT strip('a:1A,2B b:3C'::tsvector),
       setweight('a:1,2 b:3'::tsvector, 'B'),
       ts_delete('a:1 b:2'::tsvector, 'a'),
       ts_filter('a:1A,2B b:3C'::tsvector, ARRAY['A','C']::"char"[]);
SELECT ts_rewrite('a & (b | a:*A)'::tsquery, 'a'::tsquery, 'c'::tsquery),
       querytree('a & !b'::tsquery), numnode('a & !b'::tsquery);
CREATE TABLE full_text_rewrite_values(target tsquery, substitute tsquery);
INSERT INTO full_text_rewrite_values VALUES ('a', 'c'), ('b', 'd');
SELECT ts_rewrite(
  'a & b'::tsquery,
  'SELECT target, substitute FROM full_text_rewrite_values ORDER BY target');
DROP TABLE full_text_rewrite_values;
SELECT lexeme, positions, weights
FROM unnest('a:1A,2 b:3C'::tsvector) ORDER BY lexeme;
CREATE TABLE full_text_stat_values(value tsvector);
INSERT INTO full_text_stat_values VALUES
  ('a:1A,2B b:3C'), ('a c'), ('d');
SELECT * FROM ts_stat('SELECT value FROM full_text_stat_values');
SELECT * FROM ts_stat('SELECT value FROM full_text_stat_values', 'A');
DROP TABLE full_text_stat_values;
SELECT to_tsvector('simple', '{"a":"cat dog","b":["rat","mouse"]}'::jsonb),
       jsonb_to_tsvector(
         'simple', '{"a":"cat dog","b":42,"c":true}'::jsonb,
         '["key","numeric","boolean"]'::jsonb);
SELECT ts_headline(
         'simple', '{"a":"cat dog", "n": 1, "x":["rat cat"]}'::json,
         'cat'::tsquery),
       ts_headline(
         'simple', '{"a":"cat dog", "n": 1, "x":["rat cat"]}'::jsonb,
         'cat'::tsquery);

CREATE TABLE full_text_index_values(
  id integer PRIMARY KEY,
  body text NOT NULL,
  terms tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED,
  query tsquery);
INSERT INTO full_text_index_values(id, body, query) VALUES
  (1, 'The Fat Rats', plainto_tsquery('english', 'fat rat')),
  (2, 'Slow Cats', plainto_tsquery('english', 'cat'));
CREATE INDEX full_text_terms_idx ON full_text_index_values(terms);
CREATE INDEX full_text_query_idx ON full_text_index_values(query);
CREATE INDEX full_text_expression_idx ON full_text_index_values
  ((to_tsvector('english', body)));
SELECT terms, query FROM full_text_index_values ORDER BY terms, query;
SELECT indexname FROM pg_indexes
WHERE tablename = 'full_text_index_values' ORDER BY indexname;
DROP TABLE full_text_index_values;

CREATE SCHEMA full_text_diff;
CREATE TEXT SEARCH PARSER full_text_diff.default_copy (
  START = prsd_start, GETTOKEN = prsd_nexttoken, END = prsd_end,
  HEADLINE = prsd_headline, LEXTYPES = prsd_lextype);
ALTER TEXT SEARCH PARSER full_text_diff.default_copy RENAME TO parser_v2;
CREATE TEXT SEARCH TEMPLATE full_text_diff.simple_copy (
  INIT = dsimple_init, LEXIZE = dsimple_lexize);
ALTER TEXT SEARCH TEMPLATE full_text_diff.simple_copy RENAME TO simple_v2;
CREATE TEXT SEARCH DICTIONARY full_text_diff.words (
  TEMPLATE = full_text_diff.simple_v2, ACCEPT = true);
ALTER TEXT SEARCH DICTIONARY full_text_diff.words (ACCEPT = true);
CREATE TEXT SEARCH CONFIGURATION full_text_diff.documents (
  PARSER = full_text_diff.parser_v2);
ALTER TEXT SEARCH CONFIGURATION full_text_diff.documents ADD MAPPING
  FOR asciiword, word, numword, uint WITH full_text_diff.words;
COMMENT ON TEXT SEARCH CONFIGURATION full_text_diff.documents
  IS 'differential search configuration';

SELECT to_tsvector('full_text_diff.documents', 'Cats 42'),
       ts_lexize('full_text_diff.words'::regdictionary, 'Cats');
SELECT tokid, token
FROM ts_parse('full_text_diff.parser_v2', 'foo@example.com');
SELECT (SELECT count(*) FROM pg_ts_parser WHERE prsname = 'parser_v2'),
       (SELECT count(*) FROM pg_ts_template WHERE tmplname = 'simple_v2'),
       (SELECT count(*) FROM pg_ts_dict WHERE dictname = 'words'),
       (SELECT count(*) FROM pg_ts_config WHERE cfgname = 'documents');

CREATE VIEW full_text_diff.document_terms AS
  SELECT to_tsvector('full_text_diff.documents', 'Cats') AS terms;
ALTER TEXT SEARCH CONFIGURATION full_text_diff.documents
  RENAME TO documents_v2;
SELECT terms FROM full_text_diff.document_terms;
DROP TEXT SEARCH CONFIGURATION full_text_diff.documents_v2;
DROP TEXT SEARCH CONFIGURATION full_text_diff.documents_v2 CASCADE;
SELECT count(*) FROM pg_views
WHERE schemaname = 'full_text_diff' AND viewname = 'document_terms';
DROP TEXT SEARCH DICTIONARY full_text_diff.words;
DROP TEXT SEARCH TEMPLATE full_text_diff.simple_v2;
DROP TEXT SEARCH PARSER full_text_diff.parser_v2;
DROP SCHEMA full_text_diff;
