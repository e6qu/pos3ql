-- Record values compare with record_eq/record_cmp, where NULL fields are
-- equal; two row constructors compare field by field with SQL NULL rules.
CREATE TABLE rc184(a int, b text);
INSERT INTO rc184 VALUES (1, NULL), (NULL, NULL), (2, 'x');
SELECT rc184.* = rc184.*, rc184.* <> rc184.*, rc184.* < rc184.*, rc184 <= rc184,
       rc184.* IS DISTINCT FROM rc184.* FROM rc184 ORDER BY a;
SELECT ROW(rc184.*) = ROW(rc184.*), (a, b) = (a, b), rc184 = (a, b) FROM rc184 ORDER BY a;
SELECT ROW(1, NULL) = ROW(1, NULL), ROW(1, NULL) <> ROW(1, NULL), ROW(1, NULL) < ROW(2, NULL),
       (1, NULL::int) < (1, 2), (2, NULL::int) > (1, 2),
       ROW(1, NULL) IS DISTINCT FROM ROW(1, NULL), ROW(1, NULL) IS DISTINCT FROM ROW(1, 2),
       ROW(1, NULL) IS NOT DISTINCT FROM ROW(1, NULL),
       ((1, NULL::int), 2) IS DISTINCT FROM ((1, NULL::int), 2);
SELECT rc184 = ROW(1, NULL::text), rc184.* = ROW(1, NULL::text),
       (SELECT x FROM rc184 x WHERE a = 1) = (SELECT x FROM rc184 x WHERE a = 1)
FROM rc184 ORDER BY a;
SELECT x = y, x < y, x <> y, x IS DISTINCT FROM y FROM (SELECT rc184 x, rc184 y FROM rc184) s;
SELECT (SELECT ROW(1, NULL::int)) < (SELECT ROW(1, 2)), (SELECT ROW(1, 2)) < (SELECT ROW(1, NULL::int)),
       (SELECT ROW(1, 'NaN'::float8)) = (SELECT ROW(1, 'NaN'::float8)),
       NULLIF(ROW(1, NULL::int), ROW(1, NULL::int)) IS NULL,
       CASE ROW(1, NULL::int) WHEN ROW(1, NULL::int) THEN 'y' ELSE 'n' END;
SELECT (1, NULL) IN ((1, NULL), (2, 3)), (1, NULL::int) IN (SELECT 1, NULL::int),
       (SELECT (1, NULL::int)) IN (SELECT (1, NULL::int)),
       (SELECT (1, NULL::int)) = ANY (SELECT (1, NULL::int)),
       ROW(1, NULL::int) = ANY (SELECT 1, NULL::int), ROW(1) IN (SELECT 1);
SELECT rc184 IN (SELECT rc184 FROM rc184), rc184.* = ANY (SELECT rc184 FROM rc184),
       rc184.* IN (rc184.*) FROM rc184 ORDER BY a;
SELECT count(*) FROM rc184 r1 JOIN rc184 r2 ON r1 = r2;
SELECT count(*) FROM rc184 r1 JOIN rc184 r2 ON (r1.a, r1.b) = (r2.a, r2.b);
SELECT rc184 = ROW(1, NULL) FROM rc184;
SELECT (SELECT ROW(1, NULL)) = ROW(1, NULL);
SELECT ((1, NULL), 2) = ((1, NULL), 2);
SELECT (SELECT ROW(1, 2)) = ROW(1, 'a'::text);
SELECT (SELECT ROW(1, 2)) = (SELECT ROW(1, 2, 3));
SELECT (SELECT ROW(1, 2)) = (SELECT ROW(2, 2, 3));
DROP TABLE rc184;
