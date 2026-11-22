-- `@@@` is strategy 2 of GIN's `tsvector_ops`, so a GIN index serves it as
-- it serves `@@`; GiST's `tsvector_ops` lists only `@@`. Both index scans
-- and the sequential scan give the same rows.
CREATE TABLE docs (id int, v tsvector);
INSERT INTO docs SELECT g, to_tsvector('simple', 'w' || g || ' common')
FROM generate_series(1, 2000) g;
CREATE INDEX docs_v_gin ON docs USING gin (v);
ANALYZE docs;
SET enable_seqscan = off;
SELECT id FROM docs WHERE v @@@ to_tsquery('simple', 'w7');
SELECT id FROM docs WHERE to_tsquery('simple', 'w8') @@@ v;
SELECT count(*) FROM docs WHERE v @@@ to_tsquery('simple', 'common');
DROP INDEX docs_v_gin;
CREATE INDEX docs_v_gist ON docs USING gist (v);
SELECT id FROM docs WHERE v @@@ to_tsquery('simple', 'w7');
SELECT id FROM docs WHERE to_tsquery('simple', 'w7 | w9') @@@ v ORDER BY id;
RESET enable_seqscan;
DROP TABLE docs;
