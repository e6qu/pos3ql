-- PostgreSQL 18 built-in GiST/SP-GiST ordering operators. The result rows,
-- NULL placement, prepared origins, filters, and ties are the compatibility
-- boundary; physical page layout is deliberately not compared.
CREATE TABLE knn_gist_rows (
    id integer,
    location point,
    bounds box,
    shape polygon,
    radius circle,
    active boolean,
    payload text
);
CREATE INDEX knn_gist_location ON knn_gist_rows USING gist (location)
    INCLUDE (id, payload);
CREATE INDEX knn_gist_bounds ON knn_gist_rows USING gist (bounds);
CREATE INDEX knn_gist_shape ON knn_gist_rows USING gist (shape);
CREATE INDEX knn_gist_radius ON knn_gist_rows USING gist (radius);

CREATE TABLE knn_spgist_rows (
    id integer,
    quad point,
    kd point,
    bounds box,
    shape polygon,
    active boolean,
    payload text
);
CREATE INDEX knn_spgist_quad ON knn_spgist_rows USING spgist (quad)
    INCLUDE (id, payload);
CREATE INDEX knn_spgist_kd ON knn_spgist_rows USING spgist (kd kd_point_ops);
CREATE INDEX knn_spgist_bounds ON knn_spgist_rows USING spgist (bounds);
CREATE INDEX knn_spgist_shape ON knn_spgist_rows USING spgist (shape);

INSERT INTO knn_gist_rows VALUES
    (1, '(1,0)', '(1,1),(0,0)', '((1,0),(2,0),(1,1))', '<(1,0),0.25>', true, 'one'),
    (2, '(4,0)', '(5,1),(4,0)', '((4,0),(5,0),(4,1))', '<(4,0),0.25>', true, 'two'),
    (3, '(-2,0)', '(-1,1),(-2,0)', '((-2,0),(-1,0),(-2,1))', '<(-2,0),0.25>', false, 'three'),
    (4, NULL, NULL, NULL, NULL, true, 'null'),
    (5, '(0,1)', '(0,2),(-1,1)', '((0,1),(1,1),(0,2))', '<(0,1),0.25>', true, 'five');
INSERT INTO knn_spgist_rows
SELECT id, location, location, bounds, shape, active, payload FROM knn_gist_rows;

-- An unknown string literal must receive the point type from the ordering
-- operator just as it does through PostgreSQL's normal expression path.
SELECT id FROM knn_gist_rows ORDER BY location <-> '(0,0)', id;
SELECT id FROM knn_gist_rows ORDER BY bounds <-> point '(0,0)', id LIMIT 3;
SELECT id FROM knn_gist_rows ORDER BY shape <-> point '(0,0)', id LIMIT 3;
SELECT id FROM knn_gist_rows ORDER BY radius <-> point '(0,0)', id LIMIT 3;
SELECT id FROM knn_gist_rows WHERE active
 ORDER BY location <-> point '(0,0)', id LIMIT 3;
-- A residual filter is evaluated before the limit. Implementations may use a
-- ranked index only if filtered-out nearer rows cannot shorten the result.
SELECT id FROM knn_gist_rows WHERE active
 ORDER BY location <-> point '(3,0)' LIMIT 3;

SELECT id FROM knn_spgist_rows ORDER BY quad <-> point '(0,0)', id LIMIT 3;
SELECT id FROM knn_spgist_rows ORDER BY kd <-> point '(0,0)', id LIMIT 3;
SELECT id FROM knn_spgist_rows ORDER BY bounds <-> point '(0,0)', id LIMIT 3;
SELECT id FROM knn_spgist_rows ORDER BY shape <-> point '(0,0)', id LIMIT 3;

PREPARE nearest_gist(point, integer) AS
    SELECT id FROM knn_gist_rows ORDER BY location <-> $1 LIMIT $2;
EXECUTE nearest_gist(point '(3,0)', 3);
EXECUTE nearest_gist(point '(-3,0)', 2);
DEALLOCATE nearest_gist;

PREPARE nearest_window(point, integer, integer) AS
    SELECT id FROM knn_gist_rows ORDER BY location <-> $1 LIMIT $2 OFFSET $3;
EXECUTE nearest_window(point '(3,0)', 2, 1);
DEALLOCATE nearest_window;

-- These valid orders are deliberately outside the built-in ordering-operator
-- path but must retain ordinary PostgreSQL result semantics.
SELECT id FROM knn_gist_rows
 ORDER BY location <-> point '(0,0)' DESC NULLS FIRST, id LIMIT 3;
SELECT id FROM knn_gist_rows
 ORDER BY location <-> circle '<(0,0),1>', id LIMIT 3;

UPDATE knn_gist_rows SET location = '(0.25,0)' WHERE id = 2 RETURNING id;
SELECT id FROM knn_gist_rows ORDER BY location <-> point '(0,0)', id LIMIT 3;
DELETE FROM knn_spgist_rows WHERE id = 1 RETURNING id;
SELECT id FROM knn_spgist_rows ORDER BY kd <-> point '(0,0)', id LIMIT 3;

DROP TABLE knn_spgist_rows;
DROP TABLE knn_gist_rows;
