-- Array slice subscripting `a[lo:hi]` (with either bound optional), matching
-- PostgreSQL 18. 1-based and inclusive; bounds clamp to the array, an empty
-- overlap is an empty array, and a NULL bound yields NULL.
DROP TABLE IF EXISTS asl;

-- Both bounds, and each bound omitted.
SELECT (ARRAY[1,2,3,4,5])[2:4];
SELECT (ARRAY[1,2,3,4,5])[:3];
SELECT (ARRAY[1,2,3,4,5])[3:];
SELECT (ARRAY[1,2,3,4,5])[:];

-- Out-of-range bounds clamp; a lower bound below 1 clamps to 1.
SELECT (ARRAY[1,2,3])[2:10];
SELECT (ARRAY[1,2,3])[0:2];
SELECT (ARRAY[1,2,3])[5:10];
SELECT (ARRAY[1,2,3])[3:1];

-- A NULL bound makes the whole slice NULL.
SELECT (ARRAY[1,2,3])[NULL:2] IS NULL;

-- Works on text arrays, and the result keeps the array type.
SELECT (ARRAY['a','b','c','d'])[2:3];
SELECT pg_typeof((ARRAY[1,2,3])[1:2]);
SELECT array_length((ARRAY[1,2,3,4,5])[2:4], 1);

-- Composes with || and other array functions.
SELECT (ARRAY[1,2,3])[1:2] || (ARRAY[4,5])[1:1];
SELECT array_agg(x) FROM unnest((ARRAY[10,20,30,40])[2:3]) AS x;

-- On a column, alongside a plain index.
CREATE TABLE asl (a int[]);
INSERT INTO asl VALUES (ARRAY[10,20,30,40]);
SELECT a[2:3], a[1], a[2:] FROM asl;

DROP TABLE asl;
