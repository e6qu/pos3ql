-- A set-returning function in the SELECT list of an IN / ANY / ALL / ARRAY
-- subquery expands to the set of rows, matching PostgreSQL 18. (SRFs in
-- derived-table, scalar, and EXISTS subqueries already worked.)
DROP TABLE IF EXISTS sr;

CREATE TABLE sr (id int, v int);
INSERT INTO sr VALUES (1, 10), (2, 20), (3, 30);

-- IN over an unnest / generate_series subquery.
SELECT id FROM sr WHERE id IN (SELECT unnest(ARRAY[1, 3])) ORDER BY id;
SELECT id FROM sr WHERE id IN (SELECT generate_series(1, 2)) ORDER BY id;

-- = ANY and = ALL over an SRF subquery.
SELECT id FROM sr WHERE id = ANY (SELECT unnest(ARRAY[1, 3])) ORDER BY id;
SELECT id FROM sr WHERE id = ALL (SELECT unnest(ARRAY[1])) ORDER BY id;

-- NOT IN over an SRF subquery (empty-vs-NULL semantics unchanged).
SELECT id FROM sr WHERE id NOT IN (SELECT unnest(ARRAY[2])) ORDER BY id;
SELECT id FROM sr WHERE id IN (SELECT unnest(ARRAY[]::int[])) ORDER BY id;

-- An expression wrapping the SRF, and the ARRAY(subquery) constructor.
SELECT id FROM sr WHERE v IN (SELECT unnest(ARRAY[10, 30]) * 1) ORDER BY id;
SELECT array(SELECT unnest(ARRAY[5, 6]) ORDER BY 1);

DROP TABLE sr;
