-- A stored expression keeps a named object's identifier, but prints the
-- constant by name as PostgreSQL does, so pg_dump output restores.
CREATE SCHEMA bo359;
CREATE TEXT SEARCH CONFIGURATION bo359.documents (COPY = simple);
CREATE TABLE bo359.docs (
  body text,
  terms tsvector GENERATED ALWAYS AS (to_tsvector('bo359.documents', body)) STORED,
  config regconfig DEFAULT 'bo359.documents',
  CHECK (to_tsvector('bo359.documents'::regconfig, body) IS NOT NULL));
CREATE INDEX bo359_terms ON bo359.docs ((to_tsvector('bo359.documents', body)));
SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef
 WHERE adrelid = 'bo359.docs'::regclass ORDER BY adnum;
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'bo359.docs'::regclass;
SELECT pg_get_indexdef('bo359.bo359_terms'::regclass);
ALTER TEXT SEARCH CONFIGURATION bo359.documents RENAME TO renamed;
SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef
 WHERE adrelid = 'bo359.docs'::regclass ORDER BY adnum;
SELECT pg_get_indexdef('bo359.bo359_terms'::regclass);
DROP SCHEMA bo359 CASCADE;
