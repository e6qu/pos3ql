-- PostgreSQL 18 predicates served by navigable GIN and full-text GiST generations.
\pset null 'NULL'

CREATE TABLE inverted_navigation (
    id integer PRIMARY KEY,
    tags integer[],
    gin_document tsvector,
    gist_document tsvector,
    json_ops jsonb,
    json_path jsonb
);
CREATE INDEX inverted_navigation_tags ON inverted_navigation USING gin (tags);
CREATE INDEX inverted_navigation_gin_document
    ON inverted_navigation USING gin (gin_document);
CREATE INDEX inverted_navigation_gist_document
    ON inverted_navigation USING gist (gist_document);
CREATE INDEX inverted_navigation_json_ops
    ON inverted_navigation USING gin (json_ops);
CREATE INDEX inverted_navigation_json_path
    ON inverted_navigation USING gin (json_path jsonb_path_ops);

INSERT INTO inverted_navigation VALUES
    (1, ARRAY[1,2,NULL],
     to_tsvector('simple', 'alpha beta'), to_tsvector('simple', 'alpha beta'),
     '{"kind":"book","tags":["alpha","beta"],"active":true}'::jsonb,
     '{"token":"alpha","nested":{"active":true}}'::jsonb),
    (2, ARRAY[2,3],
     to_tsvector('simple', 'alphabet gamma'), to_tsvector('simple', 'alphabet gamma'),
     '{"kind":"film","tags":["gamma"],"active":false}'::jsonb,
     '{"token":"gamma","nested":{"active":false}}'::jsonb),
    (3, ARRAY[]::integer[],
     to_tsvector('simple', 'delta'), to_tsvector('simple', 'delta'),
     jsonb_build_object(E'line\nkey', 'escaped'),
     '{"token":"delta","nested":{}}'::jsonb),
    (4, NULL, NULL, NULL, NULL, NULL);

SELECT id FROM inverted_navigation WHERE tags @> ARRAY[2] ORDER BY id;
SELECT id FROM inverted_navigation WHERE tags && ARRAY[3,9] ORDER BY id;
SELECT id FROM inverted_navigation WHERE tags <@ ARRAY[1,2,3,NULL] ORDER BY id;
SELECT id FROM inverted_navigation WHERE tags = ARRAY[]::integer[] ORDER BY id;
SELECT id FROM inverted_navigation WHERE tags && ARRAY[]::integer[] ORDER BY id;

SELECT id FROM inverted_navigation
 WHERE gin_document @@ 'alpha'::tsquery ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE gin_document @@ to_tsquery('simple', 'alpha | delta') ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE gin_document @@ to_tsquery('simple', 'alpha & !gamma') ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE gin_document @@ 'alph:*'::tsquery ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE gist_document @@ phraseto_tsquery('simple', 'alpha beta') ORDER BY id;

SELECT id FROM inverted_navigation WHERE json_ops ? 'kind' ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_ops ?| ARRAY['missing','kind'] ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_ops ?& ARRAY['kind','active'] ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_ops ?& ARRAY[]::text[] ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_ops ? E'line\nkey' ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_ops @> '{"tags":["beta"],"active":true}'::jsonb ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_path @> '{"nested":{"active":false}}'::jsonb ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_path @? '$.nested.active ? (@ == true)'::jsonpath ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_path @@ '$.nested.active == false'::jsonpath ORDER BY id;

BEGIN;
UPDATE inverted_navigation
   SET tags=ARRAY[9],
       gin_document=to_tsvector('simple','omega'),
       gist_document=to_tsvector('simple','omega'),
       json_ops='{"kind":"updated"}'::jsonb,
       json_path='{"token":"omega"}'::jsonb
 WHERE id=2;
SELECT id FROM inverted_navigation WHERE tags @> ARRAY[9] ORDER BY id;
SELECT id FROM inverted_navigation WHERE gin_document @@ 'omega'::tsquery ORDER BY id;
SELECT id FROM inverted_navigation WHERE json_ops ? 'updated' ORDER BY id;
ROLLBACK;

SELECT id FROM inverted_navigation WHERE tags @> ARRAY[9] ORDER BY id;
SELECT id FROM inverted_navigation WHERE gin_document @@ 'gamma'::tsquery ORDER BY id;
REINDEX TABLE inverted_navigation;
SELECT id FROM inverted_navigation
 WHERE gist_document @@ 'gamma'::tsquery ORDER BY id;
SELECT id FROM inverted_navigation
 WHERE json_path @> '{"token":"gamma"}'::jsonb ORDER BY id;

DROP TABLE inverted_navigation;
