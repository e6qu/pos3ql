-- Exact-token GIN posting candidates and conservative sequential fallbacks.
\pset null 'NULL'

CREATE TABLE gin_postings (
    id integer PRIMARY KEY,
    tags integer[],
    document tsvector,
    json_ops jsonb,
    json_path jsonb
);
CREATE INDEX gin_postings_tags ON gin_postings USING gin (tags);
CREATE INDEX gin_postings_document ON gin_postings USING gin (document);
CREATE INDEX gin_postings_json_ops ON gin_postings USING gin (json_ops);
CREATE INDEX gin_postings_json_path
    ON gin_postings USING gin (json_path jsonb_path_ops);

INSERT INTO gin_postings VALUES
    (1, ARRAY[1,2,2], to_tsvector('simple', 'alpha beta beta'),
     '{"kind":"book","active":true,"labels":["alpha","beta"]}',
     '{"kind":"book","nested":{"active":true},"score":1}'),
    (2, ARRAY[2,3], to_tsvector('simple', 'alphabet gamma'),
     '{"kind":"film","active":false,"labels":["gamma"]}',
     '{"kind":"film","nested":{"active":false},"score":2}'),
    (3, ARRAY[4], to_tsvector('simple', 'delta'),
     '{"kind":"book","active":false,"labels":[]}',
     '{"kind":"book","nested":{},"score":3}'),
    (4, ARRAY[]::integer[], ''::tsvector, '{}'::jsonb, '{}'::jsonb),
    (5, NULL, NULL, NULL, NULL);

-- Required-token probes use one posting list; duplicate indexed tokens must
-- not duplicate result rows.
SELECT id FROM gin_postings WHERE tags @> ARRAY[2,2] ORDER BY id;
SELECT id FROM gin_postings WHERE document @@ 'beta'::tsquery ORDER BY id;
SELECT id FROM gin_postings WHERE json_ops ? 'kind' ORDER BY id;
SELECT id FROM gin_postings
 WHERE json_path @> '{"nested":{"active":false}}' ORDER BY id;

-- Disjunctions union posting lists and still emit each row once.
SELECT id FROM gin_postings WHERE tags && ARRAY[1,3,9] ORDER BY id;
SELECT id FROM gin_postings
 WHERE document @@ to_tsquery('simple', 'alpha | delta') ORDER BY id;
SELECT id FROM gin_postings
 WHERE json_ops ?| ARRAY['active','missing'] ORDER BY id;

-- A positive conjunct bounds the posting candidates; the remaining boolean
-- expression is decided by the exact SQL recheck.
SELECT id FROM gin_postings
 WHERE document @@ to_tsquery('simple', 'alpha & !gamma') ORDER BY id;
SELECT id FROM gin_postings
 WHERE json_ops ?& ARRAY['kind','active'] ORDER BY id;
SELECT id FROM gin_postings
 WHERE json_ops @> '{"kind":"book","active":false}' ORDER BY id;

-- These predicates have no safe exact token. They must retain PostgreSQL
-- results through the ordinary scan instead of treating an empty posting
-- probe as proof that no row matches.
SELECT id FROM gin_postings WHERE tags = ARRAY[]::integer[] ORDER BY id;
SELECT id FROM gin_postings WHERE tags <@ ARRAY[1,2,3] ORDER BY id;
SELECT id FROM gin_postings WHERE document @@ '!gamma'::tsquery ORDER BY id;
SELECT id FROM gin_postings WHERE document @@ 'alph:*'::tsquery ORDER BY id;
SELECT id FROM gin_postings WHERE json_ops @> '{}'::jsonb ORDER BY id;
SELECT id FROM gin_postings WHERE json_path @> '2'::jsonb ORDER BY id;
SELECT id FROM gin_postings WHERE json_ops ?& ARRAY[]::text[] ORDER BY id;

PREPARE gin_posting_array(integer[]) AS
    SELECT id FROM gin_postings WHERE tags && $1 ORDER BY id;
EXECUTE gin_posting_array(ARRAY[2,4]);
EXECUTE gin_posting_array(ARRAY[]::integer[]);
DEALLOCATE gin_posting_array;

BEGIN;
UPDATE gin_postings
   SET tags=ARRAY[8,9],
       document=to_tsvector('simple', 'omega'),
       json_ops='{"kind":"updated","active":true}',
       json_path='{"kind":"updated","nested":{"active":true}}'
 WHERE id=2;
DELETE FROM gin_postings WHERE id=3;
SELECT id FROM gin_postings WHERE tags && ARRAY[4,9] ORDER BY id;
SELECT id FROM gin_postings WHERE document @@ 'omega | delta'::tsquery ORDER BY id;
SELECT id FROM gin_postings WHERE json_ops ? 'updated' ORDER BY id;
ROLLBACK;

SELECT id FROM gin_postings WHERE tags && ARRAY[4,9] ORDER BY id;
SELECT id FROM gin_postings WHERE document @@ 'gamma | delta'::tsquery ORDER BY id;
REINDEX TABLE gin_postings;
SELECT id FROM gin_postings WHERE json_ops ?| ARRAY['kind','missing'] ORDER BY id;
SELECT id FROM gin_postings
 WHERE json_path @> '{"nested":{"active":false}}' ORDER BY id;

DROP TABLE gin_postings;
